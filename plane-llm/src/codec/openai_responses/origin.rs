use super::*;
use crate::codec::ir::{IrBlockOrigin, IrBlockSource};

pub(super) const INSTRUCTIONS: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "instructions",
};
pub(super) const INPUT_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input",
};
pub(super) const INPUT_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=input_text",
};
pub(super) const INPUT_IMAGE_ITEM: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=input_image",
};
pub(super) const OUTPUT_TEXT_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=output_text",
};
pub(super) const FUNCTION_CALL_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call",
};
pub(super) const FUNCTION_OUTPUT_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output",
};
pub(super) const REASONING_INPUT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=reasoning",
};
pub(super) const MESSAGE_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content",
};
pub(super) const SYSTEM_PART_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].text",
};
pub(super) const TOOL_OUTPUT_STRING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output",
};
pub(super) const RESPONSE_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=message.content[].type=output_text",
};
pub(super) const RESPONSE_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=message.content[].type=refusal",
};
pub(super) const RESPONSE_FUNCTION: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=function_call",
};
pub(super) const RESPONSE_REASONING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=reasoning",
};
pub(super) const RESPONSE_WEB_SEARCH: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "output[].type=web_search_call",
};
pub(super) const STREAM_FUNCTION: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_item.added.item.type=function_call",
};
pub(super) const STREAM_REASONING: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_item.added.item.type=reasoning",
};
pub(super) const STREAM_REASONING_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.reasoning_text.delta.delta",
};
pub(super) const STREAM_REASONING_SUMMARY: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.reasoning_summary_text.delta.delta",
};
pub(super) const STREAM_TEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.output_text.delta.delta",
};
pub(super) const STREAM_REFUSAL_DELTA: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.refusal.delta.delta",
};
pub(super) const STREAM_REFUSAL_DONE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.refusal.done.refusal",
};
pub(super) const STREAM_REFUSAL_COMPLETED_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.completed.response.output[].content[].type=refusal",
};
const MESSAGE_INPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_text",
};
const MESSAGE_OUTPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=output_text",
};
const MESSAGE_IMAGE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_image",
};
const MESSAGE_FILE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_file",
};
const MESSAGE_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=refusal",
};
const MESSAGE_AUDIO: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].content[].type=input_audio",
};
const TOOL_OUTPUT_INPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_text",
};
const TOOL_OUTPUT_OUTPUTTEXT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=output_text",
};
const TOOL_OUTPUT_IMAGE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_image",
};
const TOOL_OUTPUT_FILE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_file",
};
const TOOL_OUTPUT_REFUSAL: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=refusal",
};
const TOOL_OUTPUT_AUDIO: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "input[].type=function_call_output.output[].type=input_audio",
};

#[derive(Clone, Copy)]
pub(super) enum PartSite {
    Message,
    ToolOutput,
}
pub(super) fn part(site: PartSite, native_tag: &str) -> IrBlockOrigin {
    let source = match (site, native_tag) {
        (PartSite::Message, CONTENT_TYPE_INPUT_TEXT) => &MESSAGE_INPUTTEXT,
        (PartSite::Message, CONTENT_TYPE_OUTPUT_TEXT) => &MESSAGE_OUTPUTTEXT,
        (PartSite::Message, INPUT_IMAGE) => &MESSAGE_IMAGE,
        (PartSite::Message, INPUT_FILE) => &MESSAGE_FILE,
        (PartSite::Message, keys::REFUSAL) => &MESSAGE_REFUSAL,
        (PartSite::Message, keys::INPUT_AUDIO) => &MESSAGE_AUDIO,
        (PartSite::ToolOutput, CONTENT_TYPE_INPUT_TEXT) => &TOOL_OUTPUT_INPUTTEXT,
        (PartSite::ToolOutput, CONTENT_TYPE_OUTPUT_TEXT) => &TOOL_OUTPUT_OUTPUTTEXT,
        (PartSite::ToolOutput, INPUT_IMAGE) => &TOOL_OUTPUT_IMAGE,
        (PartSite::ToolOutput, INPUT_FILE) => &TOOL_OUTPUT_FILE,
        (PartSite::ToolOutput, keys::REFUSAL) => &TOOL_OUTPUT_REFUSAL,
        (PartSite::ToolOutput, keys::INPUT_AUDIO) => &TOOL_OUTPUT_AUDIO,
        _ => unreachable!("only a successfully decoded native part has an origin"),
    };
    IrBlockOrigin::at(source)
}

pub(super) const STREAM_REFUSAL_FAILED_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.failed.response.output[].content[].type=refusal",
};
pub(super) const STREAM_REFUSAL_INCOMPLETE_SNAPSHOT: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "response.incomplete.response.output[].content[].type=refusal",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ir::{IrBlock, StreamDecodeState};
    use serde_json::json;

    #[test]
    fn request_folds_and_filtered_function_output_keep_native_sources() {
        let request = ResponsesReader
            .read_request(&json!({
                "model": "fixture-model", "instructions": "same instruction",
                "input": [
                    {"type": "message", "role": "developer", "content": "same instruction"},
                    {"type": "function_call_output", "call_id": "fixture-call", "output": [
                        {"type": "future_fixture", "payload": true},
                        {"type": "input_file", "file_id": "fixture-file"},
                        {"type": "input_text", "text": "fixture answer"}
                    ]}
                ]
            }))
            .expect("native request");
        let paths: Vec<_> = request
            .block_origins
            .system
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(paths, vec!["instructions", "input[].content"]);
        assert_eq!(request.system.len(), paths.len());
        let message = request.messages.first().unwrap();
        let IrBlock::ToolResult { content, .. } = message.content.first().unwrap() else {
            panic!("fixture tool result")
        };
        let origin = request
            .block_origins
            .messages
            .first()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(
            origin.source.unwrap().path,
            "input[].type=function_call_output"
        );
        assert_eq!(content.len(), origin.children.len());
        let children: Vec<_> = origin
            .children
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(
            children,
            vec![
                "input[].type=function_call_output.output[].type=input_file",
                "input[].type=function_call_output.output[].type=input_text"
            ]
        );
    }

    #[test]
    fn response_item_creation_keeps_reasoning_distinct_from_message_content() {
        let response = ResponsesReader.read_response(crate::codec::test_host::HOST, &json!({
            "id": "fixture-response", "status": "completed", "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "fixture thought"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "fixture answer"}]}
            ]
        })).expect("native response");
        let paths: Vec<_> = response
            .block_origins
            .response
            .iter()
            .map(|origin| origin.source.unwrap().path)
            .collect();
        assert_eq!(response.content.len(), paths.len());
        assert_eq!(
            paths,
            vec![
                "output[].type=reasoning",
                "output[].type=message.content[].type=output_text"
            ]
        );
    }

    #[test]
    fn native_stream_open_records_the_actual_added_or_lazy_delta_container() {
        let host = crate::codec::test_host::HOST;
        let mut state = StreamDecodeState::default();
        let added = ResponsesReader.read_response_events(host, EVT_OUTPUT_ITEM_ADDED, &json!({
            "output_index": 0, "item": {"type": "function_call", "call_id": "fixture-call", "name": "fixture_function"}
        }), &mut state);
        assert!(added
            .iter()
            .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::BlockStart { .. })));
        assert_eq!(
            state
                .block_origins
                .values()
                .next()
                .unwrap()
                .source
                .unwrap()
                .path,
            "response.output_item.added.item.type=function_call"
        );
        let mut state = StreamDecodeState::default();
        let lazy = ResponsesReader.read_response_events(
            host,
            EVT_REASONING_SUMMARY_TEXT_DELTA,
            &json!({
                "output_index": 0, "delta": "fixture thought"
            }),
            &mut state,
        );
        assert!(lazy
            .iter()
            .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::BlockStart { .. })));
        assert_eq!(
            state
                .block_origins
                .values()
                .next()
                .unwrap()
                .source
                .unwrap()
                .path,
            "response.reasoning_summary_text.delta.delta"
        );
    }
    fn source_paths(origins: &[IrBlockOrigin]) -> Vec<&'static str> {
        origins
            .iter()
            .map(|origin| {
                let source = origin.source.expect("native source");
                assert_eq!(source.dialect, "responses");
                source.path
            })
            .collect()
    }

    #[test]
    fn typed_and_untyped_folds_and_failed_part_reads_keep_exact_pairs() {
        let request = ResponsesReader
            .read_request(&json!({
                "instructions": "prefix", "input": [
                    {"type": "message", "role": "developer", "content": [
                        {"type": "input_text", "text": ""},
                        {"type": "input_text", "text": "developer"},
                        {"type": "future_fixture", "payload": true}
                    ]},
                    {"role": "system", "content": [
                        {"type": "input_text", "text": "system"},
                        {"type": "future_fixture", "payload": true}
                    ]},
                    {"type": "message", "role": "user", "content": [
                        {"type": "future_fixture"}, {"type": "input_image"},
                        {"type": "input_text", "text": "question"}
                    ]},
                    {"type": "function_call_output", "call_id": "fixture-call", "output": [
                        {"type": "future_fixture"}, {"type": "input_image"},
                        {"type": "input_text", "text": "answer"}
                    ]}
                ]
            }))
            .expect("native request");
        assert_eq!(
            source_paths(&request.block_origins.system),
            vec![
                "instructions",
                "input[].content[].text",
                "input[].content[].text"
            ]
        );
        assert_eq!(request.system.len(), request.block_origins.system.len());
        assert_eq!(
            request.system_folds,
            vec![
                crate::codec::ir::IrSystemFold {
                    before: 0,
                    role: crate::codec::ir::IrSystemRole::Developer,
                    blocks: 1..2
                },
                crate::codec::ir::IrSystemFold {
                    before: 0,
                    role: crate::codec::ir::IrSystemRole::System,
                    blocks: 2..3
                },
            ]
        );
        let mut messages = request.messages.iter().zip(&request.block_origins.messages);
        let (message, origins) = messages.next().unwrap();
        assert_eq!(message.content.len(), origins.len());
        assert_eq!(
            source_paths(origins),
            vec!["input[].content[].type=input_text"]
        );
        let (message, origins) = messages.next().unwrap();
        assert_eq!(message.content.len(), origins.len());
        assert_eq!(
            source_paths(origins),
            vec!["input[].type=function_call_output"]
        );
        let IrBlock::ToolResult { content, .. } = message.content.first().unwrap() else {
            panic!("fixture tool result")
        };
        let children = &origins.first().unwrap().children;
        assert_eq!(content.len(), children.len());
        assert_eq!(
            source_paths(children),
            vec!["input[].type=function_call_output.output[].type=input_text"]
        );
        assert!(messages.next().is_none());
    }

    fn opened_index(events: &[crate::codec::ir::IrStreamEvent]) -> usize {
        events
            .iter()
            .find_map(|event| match event {
                crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .expect("native block start")
    }

    #[test]
    fn refusal_delta_and_done_keep_the_first_native_source_across_close_and_repeat() {
        let host = crate::codec::test_host::HOST;
        for (event_type, first_frame, path) in [
            (
                EVT_REFUSAL_DELTA,
                json!({"output_index": 0, "delta": "refused"}),
                "response.refusal.delta.delta",
            ),
            (
                EVT_REFUSAL_DONE,
                json!({"output_index": 0, "refusal": "refused"}),
                "response.refusal.done.refusal",
            ),
        ] {
            let mut state = StreamDecodeState::default();
            let events =
                ResponsesReader.read_response_events(host, event_type, &first_frame, &mut state);
            let index = opened_index(&events);
            let source = state.block_origins.get(&index).unwrap().source.unwrap();
            assert_eq!(source.dialect, "responses");
            assert_eq!(source.path, path);
            let done = json!({"output_index": 0, "refusal": "refused"});
            ResponsesReader.read_response_events(host, EVT_REFUSAL_DONE, &done, &mut state);
            let repeated =
                ResponsesReader.read_response_events(host, EVT_REFUSAL_DONE, &done, &mut state);
            assert!(!repeated
                .iter()
                .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::BlockStart { .. })));
            assert_eq!(
                state
                    .block_origins
                    .get(&index)
                    .unwrap()
                    .source
                    .unwrap()
                    .path,
                path
            );
        }
    }

    #[test]
    fn terminal_refusal_snapshot_origins_follow_the_actual_event_without_reopening() {
        let host = crate::codec::test_host::HOST;
        let frame = json!({"response": {"output": [{"type": "message", "content": [{"type": "refusal", "refusal": "refused"}]}]}});
        for (event_type, path) in [
            (
                EVT_RESPONSE_COMPLETED,
                "response.completed.response.output[].content[].type=refusal",
            ),
            (
                EVT_RESPONSE_FAILED,
                "response.failed.response.output[].content[].type=refusal",
            ),
            (
                EVT_RESPONSE_INCOMPLETE,
                "response.incomplete.response.output[].content[].type=refusal",
            ),
        ] {
            // Status-absent compatibility input reaches the existing snapshot path for all three
            // event names. A genuine status=failed must still take its earlier error return below.
            let mut state = StreamDecodeState::default();
            let events = ResponsesReader.read_response_events(host, event_type, &frame, &mut state);
            let index = opened_index(&events);
            let source = state.block_origins.get(&index).unwrap().source.unwrap();
            assert_eq!(source.dialect, "responses");
            assert_eq!(source.path, path);
            let repeat = ResponsesReader.read_response_events(host, event_type, &frame, &mut state);
            assert!(!repeat
                .iter()
                .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::BlockStart { .. })));
            assert_eq!(
                state
                    .block_origins
                    .get(&index)
                    .unwrap()
                    .source
                    .unwrap()
                    .path,
                path
            );
        }
        let mut failed = frame;
        failed
            .get_mut(keys::RESPONSE)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(keys::STATUS.to_string(), json!(STATUS_FAILED));
        let mut state = StreamDecodeState::default();
        let events =
            ResponsesReader.read_response_events(host, EVT_RESPONSE_FAILED, &failed, &mut state);
        assert!(events
            .iter()
            .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::Error(_))));
        assert!(state.block_origins.is_empty());
    }

    #[test]
    fn frames_without_a_native_start_do_not_invent_block_origins() {
        let host = crate::codec::test_host::HOST;
        for (event_type, frame) in [
            (
                EVT_OUTPUT_TEXT_DELTA,
                json!({"output_index": 0, "delta": ""}),
            ),
            (EVT_REFUSAL_DONE, json!({"output_index": 0, "refusal": ""})),
            (
                EVT_OUTPUT_ITEM_ADDED,
                json!({"output_index": 0, "item": {"type": "future_fixture"}}),
            ),
        ] {
            let mut state = StreamDecodeState::default();
            let events = ResponsesReader.read_response_events(host, event_type, &frame, &mut state);
            assert!(!events
                .iter()
                .any(|event| matches!(event, crate::codec::ir::IrStreamEvent::BlockStart { .. })));
            assert!(state.block_origins.is_empty());
        }
    }
    #[test]
    fn snapshot_item_type_is_not_invented_when_the_compatible_reader_did_not_require_it() {
        let host = crate::codec::test_host::HOST;
        for item in [
            json!({"content": [{"type": "refusal", "refusal": "fixture refusal"}]}),
            json!({"type": "future_fixture", "content": [{"type": "refusal", "refusal": "fixture refusal"}]}),
        ] {
            let mut state = StreamDecodeState::default();
            let events = ResponsesReader.read_response_events(
                host,
                EVT_RESPONSE_COMPLETED,
                &json!({"response": {"output": [item]}}),
                &mut state,
            );
            let index = opened_index(&events);
            let source = state.block_origins.get(&index).unwrap().source.unwrap();
            assert_eq!(source.dialect, "responses");
            assert_eq!(
                source.path,
                "response.completed.response.output[].content[].type=refusal"
            );
        }
    }
}
