// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStopReason, StreamDecodeState};
use serde_json::json;

#[test]
fn text_thinking_text_and_multiple_text_blocks_roundtrip() {
    for thinking in [false, true] {
        let writer = CohereWriter;
        let mut state = StreamDecodeState::default();
        let mut decoded = Vec::new();
        for index in 0..3 {
            let is_thinking = thinking && index == 1;
            let events = [
                IrStreamEvent::BlockStart {
                    index,
                    block: if is_thinking {
                        IrBlockMeta::Thinking { kind: None }
                    } else {
                        IrBlockMeta::Text
                    },
                    refusal: false,
                },
                IrStreamEvent::BlockDelta {
                    index,
                    delta: if is_thinking {
                        IrDelta::ThinkingDelta("thought".into())
                    } else {
                        IrDelta::TextDelta(format!("answer{index}"))
                    },
                },
                IrStreamEvent::BlockStop { index },
            ];
            for event in events {
                let (_, frame) = writer
                    .write_response_event(crate::codec::test_host::HOST, &event)
                    .unwrap();
                decoded.extend(CohereReader.read_response_events(
                    crate::codec::test_host::HOST,
                    "",
                    &frame,
                    &mut state,
                ));
            }
        }
        let starts: Vec<_> = decoded
            .iter()
            .filter_map(|event| match event {
                IrStreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [0, 1, 2]);
        let text: Vec<_> = decoded
            .iter()
            .filter_map(|event| match event {
                IrStreamEvent::BlockDelta {
                    delta: IrDelta::TextDelta(text),
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            if thinking {
                vec!["answer0", "answer2"]
            } else {
                vec!["answer0", "answer1", "answer2"]
            }
        );
        let stray = json!({"type":"content-delta","delta":{"message":{"content":{"text":"late"}}}});
        assert!(CohereReader
            .read_response_events(crate::codec::test_host::HOST, "", &stray, &mut state)
            .is_empty());
    }
}

#[test]
fn additional_text_slots_share_the_frame_cap_and_do_not_collide_with_tools() {
    let mut state = StreamDecodeState::default();
    for index in 0..=MAX_TRACKED_TOOL_FRAMES {
        let frame = json!({"type":"content-start","index":index});
        let events = CohereReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &frame,
            &mut state,
        );
        assert!(
            matches!(&events[..], [IrStreamEvent::BlockStart { index: actual, .. }] if *actual == index)
        );
        CohereReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &json!({"type":"content-end"}),
            &mut state,
        );
    }
    let refused = CohereReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"type":"content-start"}),
        &mut state,
    );
    assert!(matches!(refused.as_slice(), [IrStreamEvent::Error(_)]));
    assert_eq!(state.tool_ir_index.len(), MAX_TRACKED_TOOL_FRAMES);
    let mut state = StreamDecodeState::default();
    for (frame, expected) in [
        (json!({"type":"content-start"}), 0),
        (json!({"type":"tool-call-start","index":0}), 1),
        (json!({"type":"content-start","index":1}), 2),
        (json!({"type":"tool-call-start","index":1}), 3),
    ] {
        let events = CohereReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &frame,
            &mut state,
        );
        assert!(events.iter().any(
            |event| matches!(event, IrStreamEvent::BlockStart { index, .. } if *index == expected)
        ));
    }
}

#[test]
fn request_and_response_citations_use_block_local_character_offsets() {
    let message = json!({"role":"assistant","content":[{"type":"text","text":"héllo"},{"type":"text","text":"world"}],"citations":[{"start":5,"end":10,"text":"world","sources":[]}]});
    let request = CohereReader
        .read_request(&json!({"model":"m","messages":[message.clone()]}))
        .unwrap();
    let response = CohereReader
        .read_response(
            crate::codec::test_host::HOST,
            &json!({"message":message,"finish_reason":"COMPLETE"}),
        )
        .unwrap();
    for blocks in [&request.messages[0].content, &response.content] {
        let IrBlock::Text {
            citations: first, ..
        } = &blocks[0]
        else {
            panic!()
        };
        let IrBlock::Text {
            citations: second, ..
        } = &blocks[1]
        else {
            panic!()
        };
        assert!(first.is_empty());
        assert_eq!(
            (second[0].start_index, second[0].end_index),
            (Some(0), Some(5))
        );
        let wire = write_cohere_citations(blocks);
        assert_eq!(
            (wire[0]["start"].as_i64(), wire[0]["end"].as_i64()),
            (Some(5), Some(10))
        );
    }
}

#[test]
fn crossing_citations_split_without_stale_raw_coordinates() {
    let mut blocks = vec![IrBlock::text("hello"), IrBlock::text("world")];
    attach_cohere_citations(
        &mut blocks,
        read_cohere_citations(&json!([{"start":3,"end":8,"text":"lowor","sources":[]}])),
        &mut CohereCitationBudget::default(),
    )
    .unwrap();
    let wire = write_cohere_citations(&blocks);
    assert_eq!(
        wire,
        vec![
            json!({"start":3,"end":5,"text":"lo","sources":[]}),
            json!({"start":5,"end":8,"text":"wor","sources":[]})
        ]
    );
}

#[test]
fn safety_is_valid_v2_error_and_cached_tokens_are_separate_usage() {
    assert_eq!(write_cohere_stop_reason(IrStopReason::Safety), "ERROR");
    for (input, cached, uncached) in [(100, 80, 20), (10, 20, 0)] {
        let response = CohereReader.read_response(crate::codec::test_host::HOST, &json!({"message":{"content":[]},"usage":{"tokens":{"input_tokens":input,"output_tokens":3},"cached_tokens":cached}})).unwrap();
        assert_eq!(response.usage.input_tokens, uncached);
        assert_eq!(response.usage.cache_read_input_tokens, Some(cached));
        let mut state = StreamDecodeState::default();
        let events = CohereReader.read_response_events(crate::codec::test_host::HOST, "", &json!({"type":"message-end","delta":{"usage":{"tokens":{"input_tokens":input},"cached_tokens":cached}}}), &mut state);
        assert!(events.iter().any(|event| matches!(event, IrStreamEvent::MessageDelta { usage, .. } if usage.input_tokens == uncached && usage.cache_read_input_tokens == Some(cached))));
    }
}

#[test]
fn provider_error_signal_never_reaches_cohere_diagnostics() {
    let (capture, subscriber) = crate::codec_diagnostics::warnings();
    let event = IrStreamEvent::Error(IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("credential SECRET-CANARY".into()),
        retry_after: None,
    });
    let frame = tracing::dispatcher::with_default(&subscriber, || {
        let writer = CohereWriter;
        writer.write_response_event(crate::codec::test_host::HOST, &event)
    });
    let messages = crate::codec_diagnostics::diagnostic_texts(&capture);
    assert!(!messages.is_empty());
    assert!(messages
        .iter()
        .all(|message| !message.contains("SECRET-CANARY")));
    assert!(!serde_json::to_string(&frame)
        .unwrap()
        .contains("SECRET-CANARY"));
}

#[test]
fn content_tool_and_thinking_frame_overflow_terminate_translation() {
    for kind in ["text", "thinking", "thinking_delta", "tool"] {
        let mut translator =
            crate::codec::proto_stream::StreamTranslate::new("openai", "cohere").unwrap();
        let mut output = Vec::new();
        for index in 0..MAX_TRACKED_TOOL_FRAMES + 2 {
            let start = match kind {
                "thinking_delta" => {
                    json!({"type":"content-delta","delta":{"message":{"content":{"thinking":"x"}}}})
                }
                "thinking" => {
                    json!({"type":"content-start","delta":{"message":{"content":{"type":"thinking","thinking":"x"}}}})
                }
                "tool" => {
                    json!({"type":"tool-call-start","index":index,"delta":{"message":{"tool_calls":{"id":format!("t{index}"),"function":{"name":"f","arguments":"{}"}}}}})
                }
                _ => json!({"type":"content-start"}),
            };
            let stop = if kind == "tool" {
                json!({"type":"tool-call-end","index":index})
            } else {
                json!({"type":"content-end"})
            };
            for frame in [start, stop] {
                output.extend(
                    translator.feed(
                        crate::codec::test_host::HOST,
                        format!(
                            "event: {}\ndata: {frame}\n\n",
                            frame["type"].as_str().unwrap()
                        )
                        .as_bytes(),
                    ),
                );
            }
        }
        let end = json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":7,"output_tokens":3}}}});
        output.extend(translator.feed(
            crate::codec::test_host::HOST,
            format!("event: message-end\ndata: {end}\n\n").as_bytes(),
        ));
        output.extend(translator.finish(crate::codec::test_host::HOST));
        let body = String::from_utf8(output).unwrap();
        let frames: Vec<serde_json::Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.get("error").is_some())
                .count(),
            1,
            "{kind}"
        );
        assert!(
            !frames.iter().any(
                |frame| frame["choices"].as_array().is_some_and(|choices| choices
                    .iter()
                    .any(|choice| choice["finish_reason"] == "stop"))
            ),
            "{kind}"
        );
    }
}
