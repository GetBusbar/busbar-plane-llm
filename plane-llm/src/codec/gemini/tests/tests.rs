use super::*;
use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent, StreamDecodeState};

#[test]
fn streamed_media_does_not_change_synthesized_tool_call_ordinals() {
    let body = serde_json::json!({
        "responseId": "media-tool-turn",
        "candidates": [{"content": {"role": "model", "parts": [
            {"inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}},
            {"text": "answer"},
            {"functionCall": {"name": "first", "args": {}}},
            {"fileData": {"mimeType": "audio/wav", "fileUri": "https://example.com/audio.wav"}},
            {"functionCall": {"name": "second", "args": {}}}
        ]}, "finishReason": "STOP"}]
    });
    let buffered: Vec<String> = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("buffered response")
        .content
        .into_iter()
        .filter_map(|block| match block {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let streamed: Vec<String> = collect_stream(&[body])
        .into_iter()
        .filter_map(|event| match event {
            IrStreamEvent::BlockStart {
                block: IrBlockMeta::ToolUse { id, .. },
                ..
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(buffered.len(), 2);
    assert_eq!(
        streamed, buffered,
        "media indices are not tool-call ordinals"
    );
}

#[test]
fn streamed_generated_media_matches_buffered_blocks_and_preserves_text_offsets() {
    use crate::codec::ir::IrBlock;
    let parts = serde_json::json!([
        {"thought": true, "text": "plan"},
        {"text": "é "},
        {"inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}},
        {"inlineData": {"mimeType": "audio/pcm", "data": "YXVkaW8="}},
        {"fileData": {"mimeType": "image/png", "fileUri": "https://example.com/image.png"}},
        {"fileData": {"mimeType": "audio/wav", "fileUri": "https://example.com/audio.wav"}},
        {"text": "hi"}
    ]);
    let terminal = serde_json::json!({
        "candidates": [{
            "finishReason": "STOP",
            "citationMetadata": {"citationSources": [{
                "startIndex": 3, "endIndex": 5, "uri": "https://example.com/source"
            }]}
        }],
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 11}
    });
    let mut buffered_body = terminal.clone();
    buffered_body["candidates"][0]["content"] =
        serde_json::json!({"role": "model", "parts": parts});
    let expected: Vec<IrBlock> = GeminiReader
        .read_response(crate::codec::test_host::HOST, &buffered_body)
        .expect("buffered media")
        .content
        .into_iter()
        .filter(|b| matches!(b, IrBlock::Image { .. } | IrBlock::Media { .. }))
        .collect();
    let mut chunks: Vec<_> = parts.as_array().unwrap().iter().map(|part| {
        serde_json::json!({"candidates": [{"content": {"role": "model", "parts": [part]}}]})
    }).collect();
    chunks.push(terminal);
    let events = collect_stream(&chunks);
    let media: Vec<IrBlock> = events
        .iter()
        .filter_map(|event| match event {
            IrStreamEvent::BlockDelta {
                delta: IrDelta::MediaDelta(block),
                ..
            } => Some((**block).clone()),
            _ => None,
        })
        .collect();
    assert_eq!(expected.len(), 4);
    assert_eq!(media, expected);
    let mut open = None;
    let mut indices = Vec::new();
    for event in &events {
        match event {
            IrStreamEvent::BlockStart { index, .. } => {
                assert!(
                    open.replace(*index).is_none(),
                    "overlapping block: {events:?}"
                );
                indices.push(*index);
            }
            IrStreamEvent::BlockStop { index } => assert_eq!(open.take(), Some(*index)),
            _ => {}
        }
    }
    assert!(open.is_none());
    assert_eq!(indices, (0..7).collect::<Vec<_>>());
    assert!(events.iter().any(|event| matches!(event,
        IrStreamEvent::BlockDelta { index: 6, delta: IrDelta::CitationsDelta(citations) }
            if citations.len() == 1 && citations[0].start_index == Some(0) && citations[0].end_index == Some(2)
    )));
    assert!(events.iter().any(|event| matches!(event,
        IrStreamEvent::MessageDelta { usage, .. } if usage.input_tokens == 7 && usage.output_tokens == 11
    )));
}

#[test]
fn streamed_media_before_text_keeps_indices_unique_and_does_not_reopen_thinking() {
    let events = collect_stream(&[
        serde_json::json!({"candidates": [{"content": {"parts": [
            {"inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}}
        ]}}]}),
        serde_json::json!({"candidates": [{"content": {"parts": [
            {"thought": true, "text": "late thought"}, {"text": "answer"}
        ]}, "finishReason": "STOP"}]}),
    ]);
    let starts: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            IrStreamEvent::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![0, 1]);
    assert!(events.iter().any(|event| matches!(
        event,
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: IrDelta::MediaDelta(_)
        }
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        IrStreamEvent::BlockDelta {
            delta: IrDelta::ThinkingDelta(_),
            ..
        }
    )));
}

fn collect_stream(chunks: &[serde_json::Value]) -> Vec<IrStreamEvent> {
    let reader = GeminiReader;
    let mut state = StreamDecodeState::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            chunk,
            &mut state,
        ));
    }
    events
}

/// Regression: a streamed functionCall MUST produce a matching BlockStop for its tool block.
#[test]
fn test_stream_tool_block_is_closed() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);

    let tool_start_idx = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { name, .. },
            refusal: _,
        } if name == "get_weather" => Some(*index),
        _ => None,
    });
    let idx = tool_start_idx.expect("tool BlockStart must be emitted");

    let closed = events
        .iter()
        .any(|e| matches!(e, IrStreamEvent::BlockStop { index } if *index == idx));
    assert!(
        closed,
        "tool block {idx} was opened but never closed: {events:?}"
    );

    let starts = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
        .count();
    let stops = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStop { .. }))
        .count();
    assert_eq!(starts, stops, "unbalanced block events: {events:?}");
}

#[test]
fn gemini_text_then_tool_closes_text_block_before_opening_tool() {
    let events = collect_stream(&[
        serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Let me check. "}]}
            }]
        }),
        serde_json::json!({
            "candidates": [{
                "content": {"role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]}
            }]
        }),
    ]);
    let tool_start = events
        .iter()
        .position(|e| {
            matches!(
                e,
                IrStreamEvent::BlockStart {
                    block: IrBlockMeta::ToolUse { .. },
                    ..
                }
            )
        })
        .expect("a tool BlockStart must be emitted");
    assert!(
        events[..tool_start]
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { .. })),
        "the open text block must be closed (BlockStop) BEFORE the tool block opens: {events:?}"
    );
}

/// The `functionCall` arm is the one block-opening arm in this reader that does not close an
/// open thinking block first (its three siblings — text, citations, logprobs — all do).
#[test]
fn test_stream_function_call_closes_open_thinking_block() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"thought": true, "text": "let me think"},
                    {"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);

    let thinking_stop_pos = events
        .iter()
        .position(|e| matches!(e, IrStreamEvent::BlockStop { index: 0 }))
        .expect("thinking block (index 0) must be closed: {events:?}");
    let tool_start_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                IrStreamEvent::BlockStart {
                    block: IrBlockMeta::ToolUse { .. },
                    ..
                }
            )
        })
        .expect("tool BlockStart must be emitted: {events:?}");

    assert!(
        thinking_stop_pos < tool_start_pos,
        "thinking block must be closed BEFORE the tool block opens: {events:?}"
    );
}

/// The gemini reader's index allocator had the SAME post-terminal defect as the openai_chat reader
/// before its fix: `offset + text_base + open_tools.len()` recomputes at EVERY call, so after a
/// terminal path clears `text_index` (`.take()`) and drains `open_tools` (`mem::take`), a tool_calls
/// chunk arriving AFTER the finish restarts the formula at 0 and re-claims a slot the client
/// already has content in.
#[test]
fn gemini_tool_after_finish_chunk_claims_a_fresh_index() {
    let events = collect_stream(&[
        serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
            {"functionCall": {"name": "get_weather", "args": {}}}
        ]}}]}),
        serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
            {"text": "hi"}
        ]}}]}),
        serde_json::json!({"candidates":[{"finishReason": GEMINI_FINISH_STOP}]}),
        serde_json::json!({"candidates":[{"content":{"role":"model","parts":[
            {"functionCall": {"name": "get_time", "args": {}}}
        ]}}]}),
    ]);

    let pre_finish_tool_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { name, .. },
            refusal: _,
        } if name == "get_weather" => Some(*index),
        _ => None,
    });
    let text_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::Text,
            refusal: _,
        } => Some(*index),
        _ => None,
    });
    let post_finish_tool_index = events.iter().rev().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { name, .. },
            refusal: _,
        } if name == "get_time" => Some(*index),
        _ => None,
    });

    assert!(
        post_finish_tool_index.is_some(),
        "the post-finish functionCall must still open a block: {events:?}"
    );
    assert_ne!(
        post_finish_tool_index, text_index,
        "post-finish tool must not collide with text's index: post-finish={post_finish_tool_index:?} text={text_index:?}, events={events:?}"
    );
    assert_ne!(
        post_finish_tool_index, pre_finish_tool_index,
        "post-finish tool must not collide with the pre-finish tool's index: post-finish={post_finish_tool_index:?} pre-finish={pre_finish_tool_index:?}, events={events:?}"
    );
}

/// Regression: a Gemini stream chunk with `candidateCount > 1` (multiple candidates each
/// carrying their own `finishReason`) MUST still produce EXACTLY ONE terminal sequence —
/// one MessageDelta and one MessageStop — not one per candidate.
#[test]
fn test_stream_multiple_candidates_emit_single_terminal_sequence() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [
            {
                "content": {"role": "model", "parts": [{"text": "first"}]},
                "finishReason": GEMINI_FINISH_STOP
            },
            {
                "content": {"role": "model", "parts": [{"text": "second"}]},
                "finishReason": GEMINI_FINISH_STOP
            },
            {
                "content": {"role": "model", "parts": [{"text": "third"}]},
                "finishReason": GEMINI_FINISH_MAX_TOKENS
            }
        ]
    })]);

    let message_stops = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::MessageStop))
        .count();
    assert_eq!(
        message_stops, 1,
        "exactly one MessageStop expected regardless of candidateCount: {events:?}"
    );

    let message_deltas = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::MessageDelta { .. }))
        .count();
    assert_eq!(
        message_deltas, 1,
        "exactly one MessageDelta expected regardless of candidateCount: {events:?}"
    );

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockDelta {
                delta: IrDelta::TextDelta(t),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "first", "only candidates[0] text should be emitted");

    assert!(
        matches!(events.last(), Some(IrStreamEvent::MessageStop)),
        "MessageStop must terminate the stream: {events:?}"
    );

    let starts = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
        .count();
    let stops = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStop { .. }))
        .count();
    assert_eq!(starts, stops, "unbalanced block events: {events:?}");
}

/// Regression: text + tool in the same response use distinct, stable indices (text=0, tool=1)
/// and BOTH are closed.
#[test]
fn test_stream_text_and_tool_indices_stable_and_closed() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"text": "hello"},
                    {"functionCall": {"name": "f", "args": {}}}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);

    let text_start = events.iter().any(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Text,
                refusal: _,
            }
        )
    });
    assert!(text_start, "text block must open at index 0");

    let tool_start = events.iter().any(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 1,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            }
        )
    });
    assert!(tool_start, "tool block must open at index 1");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index: 0 })),
        "text block (0) must be closed"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index: 1 })),
        "tool block (1) must be closed"
    );
}

/// A functionCall part BEFORE the first text part
/// must NOT collide on IR index 0.
#[test]
fn test_stream_tool_before_text_no_index_collision() {
    for chunks in [
        vec![serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "f", "args": {}}},
                    {"text": "hello"}
                ]},
                "finishReason": GEMINI_FINISH_STOP
            }]
        })],
        vec![
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]}}]}),
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason": GEMINI_FINISH_STOP}]}),
        ],
    ] {
        let events = collect_stream(&chunks);

        let mut start_indices: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                IrStreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        let n = start_indices.len();
        start_indices.sort_unstable();
        start_indices.dedup();
        assert_eq!(
            start_indices.len(),
            n,
            "no two BlockStart frames may share an index; got duplicate in {events:?}"
        );

        assert!(
            events.iter().any(|e| matches!(
                e,
                IrStreamEvent::BlockStart {
                    index: 0,
                    block: IrBlockMeta::ToolUse { .. },
                    refusal: _,
                }
            )),
            "tool (first to appear) must take index 0: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                IrStreamEvent::BlockStart {
                    index: 1,
                    block: IrBlockMeta::Text,
                    refusal: _,
                }
            )),
            "text (after the tool) must take index 1: {events:?}"
        );

        assert!(events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index: 0 })));
        assert!(events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index: 1 })));
    }
}

/// Regression: tool block indices stay stable when the functionCall arrives in a different
/// chunk than the finishReason (per-chunk local reset previously corrupted this).
#[test]
fn test_stream_tool_index_stable_across_chunks() {
    let events = collect_stream(&[
        serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "f", "args": {"a": 1}}}]
                }
            }]
        }),
        serde_json::json!({
            "candidates": [{ "finishReason": GEMINI_FINISH_STOP }]
        }),
    ]);

    let start_idx = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { .. },
            refusal: _,
        } => Some(*index),
        _ => None,
    });
    let idx = start_idx.expect("tool BlockStart must be emitted");

    assert_eq!(idx, 0, "tool-only stream: tool block must take index 0");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index } if *index == idx)),
        "tool block opened in chunk 1 must be closed by finishReason in chunk 2: {events:?}"
    );
}

/// Regression: two functionCalls in a tool-only response get distinct, contiguous indices
/// (0 and 1 — no text block opened, so nothing reserves index 0) and both close.
#[test]
fn test_stream_two_tools_distinct_indices() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "a", "args": {}}},
                    {"functionCall": {"name": "b", "args": {}}}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);

    assert!(
        !events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                block: IrBlockMeta::Text,
                ..
            }
        )),
        "tool-only stream must not open a text block: {events:?}"
    );

    let mut tool_indices: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .collect();
    tool_indices.sort_unstable();
    assert_eq!(
        tool_indices,
        vec![0, 1],
        "tool-only stream: two tools must take contiguous indices 0,1"
    );

    for idx in [0usize, 1usize] {
        assert!(
            events
                .iter()
                .any(|e| matches!(e, IrStreamEvent::BlockStop { index } if *index == idx)),
            "tool block {idx} must be closed"
        );
    }
}

/// Interleaving regression: when a text block DOES open, the reserved index 0
/// must still hold, and tool blocks follow at 1..n — the fix must not regress text+tool order.
#[test]
fn test_stream_text_then_tool_keeps_text_at_zero() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"text": "hi"},
                    {"functionCall": {"name": "a", "args": {}}},
                    {"functionCall": {"name": "b", "args": {}}}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);

    assert!(
        events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Text,
                refusal: _,
            }
        )),
        "text block must open at index 0: {events:?}"
    );

    let mut tool_indices: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .collect();
    tool_indices.sort_unstable();
    assert_eq!(
        tool_indices,
        vec![1, 2],
        "text+tool stream: tools must follow text at indices 1,2: {events:?}"
    );
}

/// Regression: the tool BlockStart now BUFFERS the name and emits NO frame; a native Gemini
/// stream carries a tool call as a single `functionCall` part, so the name must NOT appear in a
/// separate opening frame (that produced a two-part split a native client never sees).
#[test]
fn test_writer_tool_blockstart_emits_no_frame() {
    let writer = GeminiWriter;
    let ev = IrStreamEvent::BlockStart {
        index: 1,
        block: IrBlockMeta::ToolUse {
            id: String::new(),
            name: "get_weather".to_string(),
        },
        refusal: false,
    };
    assert!(
        writer
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .is_none(),
        "tool BlockStart must buffer the name and emit no separate frame"
    );
}

/// The text BlockStart still produces no frame (Gemini inlines text parts).
#[test]
fn test_writer_text_blockstart_is_none() {
    let writer = GeminiWriter;
    let ev = IrStreamEvent::BlockStart {
        index: 0,
        block: IrBlockMeta::Text,
        refusal: false,
    };
    assert!(writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .is_none());
}

/// HIGH (asymmetric twin of the file_id leak): a Bedrock S3-source image (IMAGE_S3_SENTINEL
/// media_type, `data` = serialized s3Location JSON) reaching the Gemini egress must be SKIPPED,
/// NOT emitted as a corrupt `inlineData{mimeType:"image_s3"}` part that leaks the s3Location JSON
/// + a busbar fingerprint onto the Gemini wire.
#[test]
fn test_write_request_image_s3_dropped_not_corrupted() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("describe this".to_string()),
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Vendor {
                        vendor: "bedrock",
                        value: serde_json::json!({ "format": "png", "s3Location": { "uri": "s3://bucket/key.png" } }),
                    },
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
        !wire.contains("image_s3")
            && !wire.contains("s3://bucket/key.png")
            && !wire.contains("s3Location"),
        "an image_s3 image must not leak onto the Gemini wire (no corrupt inlineData part); \
             got {wire}"
    );
    assert!(
        !wire.contains("inlineData") && !wire.contains("fileData"),
        "no inlineData/fileData part may be emitted for an image_s3 image; got {wire}"
    );
    assert!(
        wire.contains("describe this"),
        "the text part must still survive; got {wire}"
    );
}

/// Helper: drive a sequence of IR events through ONE GeminiWriter (preserving its per-stream
/// tool buffer) and return only the emitted `functionCall` parts (the `{name, args}` objects),
/// in wire order.
fn collect_function_calls(events: &[IrStreamEvent]) -> Vec<serde_json::Value> {
    let writer = GeminiWriter;
    let mut calls = Vec::new();
    for ev in events {
        if let Some((_, chunk)) = writer.write_response_event(crate::codec::test_host::HOST, ev) {
            if let Some(parts) = chunk
                .pointer("/candidates/0/content/parts")
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(fc) = part.get(FIELD_FUNCTION_CALL) {
                        calls.push(fc.clone());
                    }
                }
            }
        }
    }
    calls
}

#[test]
fn streamed_tool_arguments_use_the_buffered_object_coercion() {
    for (input, expected) in [
        (
            serde_json::json!([1, 2]),
            serde_json::json!({"args": [1, 2]}),
        ),
        (serde_json::json!(7), serde_json::json!({"args": 7})),
        (serde_json::json!(null), serde_json::json!({})),
        (serde_json::json!({"x": 1}), serde_json::json!({"x": 1})),
    ] {
        let calls = collect_function_calls(&[
            IrStreamEvent::BlockStart {
                index: 1,
                block: IrBlockMeta::ToolUse {
                    id: "call_args".into(),
                    name: "f".into(),
                },
                refusal: false,
            },
            IrStreamEvent::BlockDelta {
                index: 1,
                delta: IrDelta::InputJsonDelta(input.to_string()),
            },
            IrStreamEvent::BlockStop { index: 1 },
        ]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["args"], expected, "input {input}");
        let response = crate::codec::ir::IrResponse {
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                id: "call_args".into(),
                name: "f".into(),
                input,
                cache_control: None,
                thought_signature: None,
            }],
            ..Default::default()
        };
        let writer = GeminiWriter;
        let buffered = writer.write_response(crate::codec::test_host::HOST, &response);
        assert_eq!(
            buffered["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
            expected
        );
    }
}

#[test]
fn streamed_citation_anchor_stays_a_prefix_after_overflow() {
    let writer = GeminiWriter;
    writer.set_translate_cap(4);
    for text in ["é", "too long", "x"] {
        let (_, frame) = writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: IrDelta::TextDelta(text.into()),
                },
            )
            .expect("text frame");
        assert_eq!(frame["candidates"][0]["content"]["parts"][0]["text"], text);
    }
    let (_, frame) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::CitationsDelta(vec![crate::codec::ir::IrCitation {
                    url: Some("https://example.com/source".into()),
                    start_index: Some(0),
                    end_index: Some(3),
                    ..Default::default()
                }]),
            },
        )
        .expect("citation frame");
    let source = &frame["candidates"][0]["citationMetadata"]["citationSources"][0];
    assert_eq!(source["startIndex"], 0);
    assert_eq!(source["endIndex"], 2, "clamps to retained UTF-8 prefix é");
}

/// Regression: a tool BlockStart + one whole-JSON InputJsonDelta + BlockStop emits EXACTLY ONE
/// native `functionCall` part carrying BOTH the buffered name and the args.
#[test]
fn test_writer_tool_call_emits_single_name_and_args_part() {
    let calls = collect_function_calls(&[
        IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("{\"city\":\"SF\"}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 1 },
    ]);
    assert_eq!(
        calls.len(),
        1,
        "tool call must emit exactly one functionCall part: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/name").and_then(|n| n.as_str()),
        Some("get_weather"),
        "part must carry the buffered name: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/args/city").and_then(|c| c.as_str()),
        Some("SF"),
        "part must carry the args: {calls:?}"
    );
}

/// Regression: the `arguments` JSON arrives SPLIT across MULTIPLE partial-
/// JSON InputJsonDelta fragments (the normal OpenAI/Anthropic streaming behavior — the reader
/// emits one InputJsonDelta per upstream `arguments` fragment, none coalesced).
#[test]
fn test_writer_tool_call_reassembles_split_json_args() {
    let calls = collect_function_calls(&[
        IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("{\"lo".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("c\":\"SF\",\"unit".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("\":\"C\"}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 1 },
    ]);
    assert_eq!(
        calls.len(),
        1,
        "multi-fragment args must still emit exactly ONE functionCall part: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/name").and_then(|n| n.as_str()),
        Some("get_weather"),
        "part name must be non-empty after reassembly: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/args/loc").and_then(|c| c.as_str()),
        Some("SF"),
        "args must be the FULLY reassembled object, not a partial fragment: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/args/unit").and_then(|c| c.as_str()),
        Some("C"),
        "every reassembled arg key must survive: {calls:?}"
    );
}

/// Regression: a tool call's `arguments` accumulated JSON string must not grow WITHOUT BOUND
/// across the life of a stream.
#[test]
fn test_writer_tool_call_args_capped_at_max_len() {
    let cap = crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT;
    let writer = GeminiWriter;
    writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "big_tool".to_string(),
            },
            refusal: false,
        },
    );

    let fragment = "a".repeat(1024 * 1024);
    let total_fragments = (cap / fragment.len()) + 8;
    writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("{\"blob\":\"".to_string()),
        },
    );
    for _ in 0..total_fragments {
        writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 1,
                delta: IrDelta::InputJsonDelta(fragment.clone()),
            },
        );
    }

    {
        let stream = writer.stream.lock();
        let args = &stream
            .open_tools
            .iter()
            .find(|t| t.index == 1)
            .expect("open")
            .args;
        assert!(
            args.len() <= cap,
            "accumulated args buffer must never exceed translate_body_max_bytes() ({}), got {}",
            cap,
            args.len()
        );
        assert!(
            args.len() as u64 + fragment.len() as u64 > cap as u64,
            "test must actually have driven the buffer up TO the cap, not stopped early: got {}",
            args.len()
        );
    }

    let (_, chunk) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 1 },
        )
        .expect("overflow must be reported to the client");
    let parts = chunk
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let calls: Vec<serde_json::Value> = parts
        .iter()
        .filter_map(|part| part.get(FIELD_FUNCTION_CALL).cloned())
        .collect();
    assert!(
        calls.is_empty(),
        "overflow must not produce a runnable call: {chunk}"
    );
    assert_eq!(chunk.pointer("/error/code"), Some(&serde_json::json!(500)));
    assert_eq!(
        chunk.pointer("/error/status"),
        Some(&serde_json::json!("INTERNAL"))
    );
}

/// Happy-path regression: a NORMAL-sized tool call's arguments (well under
/// `crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT`) must still round-trip correctly —
/// the cap must not
/// false-positive on legitimate multi-fragment argument streams.
#[test]
fn test_writer_tool_call_args_under_cap_round_trips() {
    let calls = collect_function_calls(&[
        IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("{\"city\":\"S".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("F\",\"notes\":\"".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 1,

            delta: IrDelta::InputJsonDelta("x".repeat(64 * 1024)),
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("\"}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 1 },
    ]);
    assert_eq!(
        calls.len(),
        1,
        "a normal-sized multi-fragment tool call must still emit exactly one functionCall part: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/name").and_then(|n| n.as_str()),
        Some("get_weather"),
        "name must round-trip: {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/args/city").and_then(|c| c.as_str()),
        Some("SF"),
        "args must be the fully reassembled object, uncorrupted by the cap: {calls:?}"
    );
    let notes = calls[0]
        .pointer("/args/notes")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    assert_eq!(
        notes.len(),
        64 * 1024,
        "the large-but-under-cap notes payload must survive intact: got {} bytes",
        notes.len()
    );
}

/// Regression: TWO parallel tool blocks in one stream, with their
/// BlockStarts NOT strictly interleaved with their own BlockStops (the OpenAI reader emits
/// BlockStart(1), BlockStart(2), then their deltas, then BlockStop(1), BlockStop(2)).
#[test]
fn test_writer_parallel_tool_calls_keep_distinct_names_and_args() {
    let calls = collect_function_calls(&[
        IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockStart {
            index: 2,
            block: IrBlockMeta::ToolUse {
                id: String::new(),
                name: "get_time".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("{\"city\":".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 2,
            delta: IrDelta::InputJsonDelta("{\"tz\":\"UTC\"}".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 1,
            delta: IrDelta::InputJsonDelta("\"SF\"}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 1 },
        IrStreamEvent::BlockStop { index: 2 },
    ]);
    assert_eq!(
        calls.len(),
        2,
        "two parallel tool calls must emit two functionCall parts: {calls:?}"
    );

    assert_eq!(
        calls[0].pointer("/name").and_then(|n| n.as_str()),
        Some("get_weather"),
        "first flushed part must keep tool 1's name (not clobbered by tool 2): {calls:?}"
    );
    assert_eq!(
        calls[0].pointer("/args/city").and_then(|c| c.as_str()),
        Some("SF"),
        "tool 1's interleaved split args must reassemble: {calls:?}"
    );
    assert_eq!(
        calls[1].pointer("/name").and_then(|n| n.as_str()),
        Some("get_time"),
        "second flushed part must keep tool 2's name: {calls:?}"
    );
    assert_eq!(
        calls[1].pointer("/args/tz").and_then(|c| c.as_str()),
        Some("UTC"),
        "tool 2's args must survive: {calls:?}"
    );
}

/// Regression: a zero-argument tool call (BlockStart then BlockStop with NO InputJsonDelta) must
/// still emit one `{name, args:{}}` part on the BlockStop flush — the call is never lost.
#[test]
fn test_writer_tool_call_empty_args_flushed_on_stop() {
    let writer = GeminiWriter;
    assert!(writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 1,
                block: IrBlockMeta::ToolUse {
                    id: String::new(),
                    name: "ping".to_string(),
                },
                refusal: false,
            }
        )
        .is_none());
    let (_, chunk) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 1 },
        )
        .expect("zero-arg tool call must flush a functionCall frame on BlockStop");
    let func = chunk
        .pointer("/candidates/0/content/parts/0/functionCall")
        .expect("functionCall part");
    assert_eq!(
        func.pointer("/name").and_then(|n| n.as_str()),
        Some("ping"),
        "flushed frame must carry the name: {chunk}"
    );
    assert!(
        func.get("args").map(|a| a.is_object()).unwrap_or(false),
        "flushed frame must carry an (empty) args object: {chunk}"
    );
}

/// Regression: neither the BlockStart nor the args delta emits a frame — the functionCall part
/// is written ONCE, on BlockStop.
#[test]
fn test_writer_tool_call_no_frame_before_block_stop() {
    let writer = GeminiWriter;
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: 1,
                    block: IrBlockMeta::ToolUse {
                        id: String::new(),
                        name: "get_weather".to_string(),
                    },
                    refusal: false,
                }
            )
            .is_none(),
        "tool BlockStart must emit no frame"
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockDelta {
                    index: 1,
                    delta: IrDelta::InputJsonDelta("{\"city\":\"SF\"}".to_string()),
                }
            )
            .is_none(),
        "args delta must accumulate, not emit a frame"
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 1 }
            )
            .is_some(),
        "BlockStop must flush the single functionCall frame"
    );
}

/// extract_error parses the body once and derives both the provider code and structured type.
#[test]
fn test_extract_error_single_parse_fields() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED"}}"#;
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, body);
    assert_eq!(raw.http_status, 429);
    assert_eq!(raw.provider_code.as_deref(), Some("429"));
    assert_eq!(
        raw.structured_type.as_deref(),
        Some(GRPC_RESOURCE_EXHAUSTED)
    );

    assert_eq!(raw.retry_after_secs, None);
}

/// An integer `error.code` (the real Gemini shape) must be stringified into
/// `provider_code` — NOT silently dropped to the gRPC status name.
#[test]
fn test_extract_error_integer_code_is_stringified() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":503,"status":"UNAVAILABLE","message":"overloaded"}}"#;
    let raw = reader.extract_error(StatusCode::SERVICE_UNAVAILABLE, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("503"),
        "integer code must be stringified, not fall back to status"
    );
    assert_eq!(raw.structured_type.as_deref(), Some(GRPC_UNAVAILABLE));
}

/// A string-typed `code` (some proxies emit one) is still accepted as the secondary path.
#[test]
fn test_extract_error_string_code_still_accepted() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":"429","status":"RESOURCE_EXHAUSTED"}}"#;
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, body);
    assert_eq!(raw.provider_code.as_deref(), Some("429"));
}

/// When `code` is absent, extract_error falls back to `status` for the provider code.
#[test]
fn test_extract_error_status_fallback() {
    let reader = GeminiReader;
    let body = br#"{"error":{"status":"PERMISSION_DENIED"}}"#;
    let raw = reader.extract_error(StatusCode::FORBIDDEN, body);
    assert_eq!(raw.provider_code.as_deref(), Some(GRPC_PERMISSION_DENIED));
    assert_eq!(raw.structured_type.as_deref(), Some(GRPC_PERMISSION_DENIED));
}

/// A real Gemini oversized-context error is a
/// 400 `INVALID_ARGUMENT` whose MESSAGE carries the token-overflow text — there is no distinct
/// google.rpc.Code for it.
#[test]
fn test_extract_error_oversized_context_yields_canonical_code() {
    let reader = GeminiReader;

    let body = br#"{"error":{"code":400,"message":"The input token count (1050000) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(raw.http_status, 400);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "oversized-context 400 must synthesize the canonical code so the breaker fails over"
    );

    assert_eq!(raw.structured_type.as_deref(), Some(GRPC_INVALID_ARGUMENT));
}

/// A second real-world phrasing the official API emits ("input is longer than the maximum number
/// of tokens") must also synthesize the canonical code — mirroring the `classify()` substring set.
#[test]
fn test_extract_error_oversized_context_alternate_phrasing() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":400,"message":"input is longer than the maximum number of tokens","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded")
    );
}

/// A NON-context-length 400 (e.g. a malformed field) must NOT be misclassified as context-length:
#[test]
fn test_extract_error_unrelated_invalid_argument_keeps_status_code() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":400,"message":"Invalid value at 'contents[0].role'.","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("400"),
        "a non-context-length 400 must not be misclassified as context_length_exceeded"
    );
}

/// Explicit bad-key ErrorInfo is preserved as an authentication signal in the raw contract.
#[test]
fn test_extract_error_bad_api_key_classifies_as_auth_harddown() {
    let reader = GeminiReader;

    let body = br#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}]}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);

    assert_eq!(
        raw.http_status, 401,
        "a dead Gemini key must re-shape to the Auth-classifying status, not relay 400"
    );
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("auth"),
        "bad-key 400 must synthesize the canonical auth provider_code"
    );

    let sig = crate::codec::test_host::classified(&raw);
    assert!(
        matches!(sig.class, StatusClass::Auth),
        "bad Gemini key must classify as Auth, got {:?}",
        sig.class
    );
    assert!(
        matches!(
            sig.class.disposition(),
            busbar_contract::upstream::Disposition::HardDown
        ),
        "a dead credential must HardDown the lane so it parks and fails over"
    );
}

/// The documented machine-readable `API_KEY_INVALID` reason can also accompany a 403
/// `PERMISSION_DENIED` (a key lacking access).
#[test]
fn test_extract_error_bad_api_key_permission_denied_is_auth() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":403,"message":"Permission denied: API key not valid.","status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}}"#;
    let raw = reader.extract_error(StatusCode::FORBIDDEN, body);
    assert_eq!(raw.http_status, 401);
    assert_eq!(raw.provider_code.as_deref(), Some("auth"));
    let sig = crate::codec::test_host::classified(&raw);
    assert!(matches!(sig.class, StatusClass::Auth));
}

/// PRECISION GUARD: the bad-key reason is read from the ErrorInfo field it is documented to live
/// in, never from the body at large.
#[test]
fn test_extract_error_echoed_api_key_invalid_token_does_not_park_the_lane() {
    let reader = GeminiReader;

    let body = br#"{"error":{"code":400,"message":"Invalid JSON payload received. Unknown name \"api_key_invalid\" at 'contents[0]': Cannot find field.","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.http_status, 400,
        "an echoed token is the caller's own text, not the credential's verdict: the real status \
         must survive"
    );
    assert_ne!(
        raw.provider_code.as_deref(),
        Some("auth"),
        "a field-validation 400 must not synthesize the auth provider_code"
    );
    let sig = crate::codec::test_host::classified(&raw);
    assert!(
        !matches!(sig.class, StatusClass::Auth),
        "a lane-healthy client error must not classify as Auth, got {:?}",
        sig.class
    );
    assert!(
        !matches!(
            sig.class.disposition(),
            busbar_contract::upstream::Disposition::HardDown
        ),
        "no caller-supplied string may park a destination for every tenant sharing it"
    );
}

/// PRECISION GUARD: a GENERIC `INVALID_ARGUMENT` 400 (a real field-validation error, no api-key
/// signal) must NOT be misclassified as auth — it stays a lane-healthy ClientFault that records
/// nothing and relays verbatim.
#[test]
fn test_extract_error_generic_invalid_argument_stays_client_fault() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":400,"message":"Invalid value at 'contents[0].role' (TYPE_ENUM), \"banana\"","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);

    assert_eq!(
        raw.http_status, 400,
        "a generic validation 400 must NOT be re-shaped to the auth status"
    );
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("400"),
        "a generic INVALID_ARGUMENT must keep its bare status code, not become auth"
    );
    let sig = crate::codec::test_host::classified(&raw);
    assert!(
        matches!(sig.class, StatusClass::ClientError),
        "a generic validation 400 must stay ClientError, got {:?}",
        sig.class
    );
    assert!(
        matches!(
            sig.class.disposition(),
            busbar_contract::upstream::Disposition::ClientFault
        ),
        "a generic validation 400 must stay a no-penalty ClientFault"
    );
}

/// PRECISION GUARD: a bare `PERMISSION_DENIED` with NO api-key text (e.g. the existing
/// status-fallback fixture shape) must NOT be re-shaped — the prose heuristic requires an explicit
/// "api key" + invalid/expired signal, so a permission error without that text stays as-is and is
/// classified by HTTP status alone.
#[test]
fn test_extract_error_bare_permission_denied_not_treated_as_bad_key() {
    let reader = GeminiReader;
    let body = br#"{"error":{"status":"PERMISSION_DENIED"}}"#;
    let raw = reader.extract_error(StatusCode::FORBIDDEN, body);

    assert_eq!(raw.http_status, 403);
    assert_eq!(raw.provider_code.as_deref(), Some(GRPC_PERMISSION_DENIED));
}

/// Malformed (non-JSON) error bodies yield None fields without panicking.
#[test]
fn test_extract_error_non_json_body() {
    let reader = GeminiReader;
    let raw = reader.extract_error(StatusCode::INTERNAL_SERVER_ERROR, b"upstream exploded");
    assert_eq!(raw.http_status, 500);
    assert_eq!(raw.provider_code, None);
    assert_eq!(raw.structured_type, None);
}

/// The native Gemini error envelope is google.rpc.Status-shaped:
#[test]
fn test_write_error_native_gemini_envelope() {
    let writer = GeminiWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        404,
        "not_found",
        "model 'x' not found",
    );

    let serialized = serde_json::to_string(&v).expect("write_error must serialize");
    let reparsed: serde_json::Value =
        serde_json::from_str(&serialized).expect("write_error must be valid JSON");
    assert_eq!(reparsed["error"]["code"], serde_json::json!(404));
    assert_eq!(
        reparsed["error"]["message"],
        serde_json::json!("model 'x' not found")
    );
    assert_eq!(reparsed["error"]["status"], serde_json::json!("NOT_FOUND"));

    assert!(
        reparsed["error"].get("type").is_none(),
        "native gemini envelope must not carry an OpenAI-style `type`: {v}"
    );
}

/// `kind` is mapped onto the google.rpc.Code vocabulary (e.g. rate-limit → RESOURCE_EXHAUSTED).
#[test]
fn test_write_error_kind_maps_to_status_vocabulary() {
    let writer = GeminiWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        429,
        "rate_limit_error",
        "slow down",
    );
    assert_eq!(v["error"]["code"], serde_json::json!(429));
    assert_eq!(
        v["error"]["status"],
        serde_json::json!("RESOURCE_EXHAUSTED") // golden wire-contract literal (kept bare on purpose)
    );

    let v = writer.write_error(
        crate::codec::test_host::HOST,
        400,
        "invalid_request_error",
        "bad",
    );
    assert_eq!(v["error"]["status"], serde_json::json!("INVALID_ARGUMENT"));
}

/// An unrecognized `kind` falls back to the HTTP-status-derived google.rpc.Code name (never a
/// non-canonical `status` string a native SDK would choke on).
#[test]
fn test_write_error_unknown_kind_falls_back_to_http_status() {
    let writer = GeminiWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        403,
        "totally_made_up_kind",
        "nope",
    );
    assert_eq!(v["error"]["status"], serde_json::json!("PERMISSION_DENIED"));

    let v = writer.write_error(
        crate::codec::test_host::HOST,
        502,
        "totally_made_up_kind",
        "bad gateway",
    );
    assert_eq!(v["error"]["status"], serde_json::json!("INTERNAL"));
}

/// The emitted `code`/`status` pair must always be an INTERNALLY CONSISTENT
/// google.rpc.Status pairing — the real Generative Language API never emits `code:503` with
/// `status:INTERNAL`.
#[test]
fn test_write_error_code_status_pair_stays_consistent() {
    let writer = GeminiWriter;

    let v = writer.write_error(
        crate::codec::test_host::HOST,
        503,
        "api_error",
        "upstream overloaded",
    );
    assert_eq!(v["error"]["code"], serde_json::json!(503));
    assert_eq!(
        v["error"]["status"],
        serde_json::json!("UNAVAILABLE"), // golden wire-contract literal (kept bare on purpose)
        "code:503 must pair with UNAVAILABLE, never INTERNAL: {v}"
    );

    let v = writer.write_error(
        crate::codec::test_host::HOST,
        503,
        "overloaded",
        "upstream overloaded",
    );
    assert_eq!(v["error"]["status"], serde_json::json!("UNAVAILABLE"));

    let v = writer.write_error(crate::codec::test_host::HOST, 500, "api_error", "boom");
    assert_eq!(v["error"]["code"], serde_json::json!(500));
    assert_eq!(v["error"]["status"], serde_json::json!("INTERNAL"));

    let v = writer.write_error(crate::codec::test_host::HOST, 504, "timeout", "slow");
    assert_eq!(v["error"]["status"], serde_json::json!("DEADLINE_EXCEEDED"));

    let v = writer.write_error(crate::codec::test_host::HOST, 403, "auth", "denied");
    assert_eq!(v["error"]["code"], serde_json::json!(403));
    assert_eq!(v["error"]["status"], serde_json::json!("PERMISSION_DENIED"));
}

/// Same-protocol (Gemini→Gemini) passthrough preserves the upstream `responseId` and
/// `modelVersion` exactly: read_response captures them, write_response emits them verbatim.
#[test]
fn test_response_identity_roundtrip_preserves_id_and_model() {
    let reader = GeminiReader;
    let writer = GeminiWriter;
    let upstream = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hi"}]},
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1},
        "modelVersion": "gemini-1.5-pro-002",
        "responseId": "abc-XYZ-123_opaque"
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &upstream)
        .expect("read_response");
    assert_eq!(ir.id.as_deref(), Some("abc-XYZ-123_opaque"));
    assert_eq!(ir.model.as_deref(), Some("gemini-1.5-pro-002"));

    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire[FIELD_RESPONSE_ID],
        serde_json::json!("abc-XYZ-123_opaque"),
        "responseId must be preserved verbatim on same-protocol passthrough: {wire}"
    );
    assert_eq!(
        wire[FIELD_MODEL_VERSION],
        serde_json::json!("gemini-1.5-pro-002"),
        "modelVersion must be preserved verbatim: {wire}"
    );

    assert!(
        wire.get("created").is_none(),
        "must not synthesize a `created` field Gemini never emits: {wire}"
    );
}

/// F2 conformance: a foreign / novel IR stop_reason (`refusal` from Responses, `error` from
/// Cohere) must NOT upper-case-leak into `finishReason` (e.g. "REFUSAL"/"ERROR" are outside
/// Gemini's `FinishReason` enum and a strict google-genai client rejects them).
#[test]
fn foreign_stop_reason_maps_to_other_not_verbatim() {
    use crate::codec::ir::IrStopReason as S;
    let writer = GeminiWriter;
    for (foreign, native) in [
        (S::Refusal, "OTHER"),
        (S::Error, "OTHER"),
        (S::Other, "OTHER"),
    ] {
        let ir = crate::codec::ir::IrResponse {
            content: vec![crate::codec::ir::IrBlock::text("x".to_string())],
            stop_reason: Some(foreign),
            usage: crate::codec::ir::IrUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            wire["candidates"][0]["finishReason"],
            serde_json::json!(native),
            "whole-body: foreign stop_reason {foreign:?} must map to {native}: {wire}"
        );
        let ev = IrStreamEvent::MessageDelta {
            stop_reason: Some(foreign),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                detail: crate::codec::ir::IrUsageDetail::default(),
            },
            stop_detail: None,
        };
        let (_, frame) = writer
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .expect("MessageDelta must emit a frame");
        assert_eq!(
            frame["candidates"][0]["finishReason"],
            serde_json::json!(native),
            "streamed: foreign stop_reason {foreign:?} must map to {native}: {frame}"
        );
    }
}

/// Cross-protocol write where a non-Gemini backend reader DID set a response id (the normal
/// cross-protocol case — OpenAI `chatcmpl-…`, Anthropic `msg_…`) emits it as `responseId`, so a
/// native `google-genai` SDK reading `GenerateContentResponse.response_id` always sees a value.
#[test]
fn test_response_identity_cross_protocol_emits_foreign_id() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hello".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        id: Some("chatcmpl-abc123".to_string()),
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire[FIELD_RESPONSE_ID],
        serde_json::json!("chatcmpl-abc123"),
        "a cross-protocol response id must surface as responseId: {wire}"
    );
}

/// Fidelity guard: when the IR carries NO id (a native Gemini body that omitted `responseId`, or
/// a backend with no identity at all), `write_response` must NOT fabricate one — emitting a
/// `responseId` would make a native passthrough distinguishable from the real response.
#[test]
fn test_response_identity_none_id_is_omitted_not_fabricated() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert!(
        wire.get(FIELD_RESPONSE_ID).is_none(),
        "must not fabricate a responseId when the IR carries none: {wire}"
    );
}

/// The streaming reader captures the stream identity from the first chunk into MessageStart,
/// and the streaming writer emits it back (synthesizing when absent) — same-protocol fidelity.
#[test]
fn test_stream_message_start_captures_and_emits_identity() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hi"}]}
        }],
        "modelVersion": "gemini-1.5-flash",
        "responseId": "stream-abc-1"
    })]);
    let start = events
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageStart { id, model, .. } => Some((id.clone(), model.clone())),
            _ => None,
        })
        .expect("MessageStart emitted");
    assert_eq!(start.0.as_deref(), Some("stream-abc-1"));
    assert_eq!(start.1.as_deref(), Some("gemini-1.5-flash"));

    let writer = GeminiWriter;
    let frame = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: start.0.clone(),
                created: None,
                model: start.1.clone(),
            },
        )
        .expect("MessageStart must emit an identity frame");
    assert_eq!(
        frame.1[FIELD_RESPONSE_ID],
        serde_json::json!("stream-abc-1"),
        "stream MessageStart frame must carry responseId: {}",
        frame.1
    );
}

/// Cross-protocol fidelity (stream): a MessageStart with NO identity (the post-strip state on a
/// cross-protocol Gemini-ingress stream — `StreamTranslate` clears the foreign id/model) must
/// still SYNTHESIZE a `responseId`, because a native google-genai SDK reads `chunk.response_id`
/// off the first chunk.
#[test]
fn test_stream_message_start_no_identity_synthesizes_response_id() {
    let writer = GeminiWriter;
    let frame = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("a synthesized-identity MessageStart must emit a frame");
    assert!(
        frame
            .1
            .get(FIELD_RESPONSE_ID)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "post-strip MessageStart must synthesize a responseId: {}",
        frame.1
    );
}

/// Regression: a NATIVE request (which never carries `stream`) must produce a body with no
/// `stream` member — i.e. the writer no longer injects one unconditionally.
#[test]
fn test_native_request_without_stream_stays_streamless() {
    let reader = GeminiReader;

    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let mut ir = reader.read_request(&body).expect("read_request");

    ir.stream = true;
    assert!(
        !ir.extra.contains_key("stream"),
        "a native request carries no stream in extra: {:?}",
        ir.extra
    );
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        wire.get("stream").is_none(),
        "stream intent must not be serialised into the body: {wire}"
    );
}

/// An `INVALID_ARGUMENT` 400 whose
/// prose contains the bare word "invalid" AND names an "api key" but is NOT a bad-key error
/// (a field-validation message that references an api-key-shaped field) must stay a lane-healthy
/// ClientFault.
#[test]
fn test_extract_error_invalid_word_near_api_key_stays_client_fault() {
    let reader = GeminiReader;

    let body = br#"{"error":{"code":400,"message":"Invalid value at 'request.api key field' (TYPE_STRING)","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.http_status, 400,
        "a generic validation 400 that merely mentions 'invalid' near 'api key' must NOT re-shape to auth"
    );
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("400"),
        "the bare status code must be preserved, not synthesized to auth"
    );
    let sig = crate::codec::test_host::classified(&raw);
    assert!(
        matches!(sig.class, StatusClass::ClientError),
        "must stay ClientError, got {:?}",
        sig.class
    );
    assert!(
        matches!(
            sig.class.disposition(),
            busbar_contract::upstream::Disposition::ClientFault
        ),
        "must stay a no-penalty ClientFault"
    );
}

/// Companion to the refinement: an EXPIRED-key prose message ("API key expired") with no
/// machine-readable reason must STILL be detected as auth — the refined phrase set covers it.
#[test]
fn test_extract_error_expired_api_key_prose_is_auth() {
    let reader = GeminiReader;
    let body = br#"{"error":{"code":400,"message":"API key expired. Please renew the API key.","status":"INVALID_ARGUMENT"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.http_status, 401,
        "an 'API key expired' prose message must re-shape to the auth status"
    );
    assert_eq!(raw.provider_code.as_deref(), Some("auth"));
}

/// A thinking-only assistant turn must NOT vanish from
/// `contents` — dropping it would collapse user/model alternation (two user turns adjacent) and
/// 400 the real Gemini API.
#[test]
fn test_write_request_thinking_only_turn_survives_with_placeholder() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("first".to_string())],
            },
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
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("second".to_string())],
            },
        ],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let contents = wire
        .get("contents")
        .and_then(|v| v.as_array())
        .expect("contents array must exist");

    assert_eq!(
        contents.len(),
        3,
        "thinking-only model turn must survive so user/model alternation is preserved: {wire}"
    );
    let model_turn = &contents[1];
    assert_eq!(
        model_turn.get("role").and_then(|v| v.as_str()),
        Some("model"),
        "the surviving turn must carry the model role: {model_turn}"
    );
    let parts = model_turn
        .get("parts")
        .and_then(|v| v.as_array())
        .expect("the surviving turn must carry a parts array");
    assert_eq!(
        parts.len(),
        1,
        "thinking-only turn carries exactly one part: {model_turn}"
    );

    assert_eq!(
        parts[0].get("text").and_then(|v| v.as_str()),
        Some("internal reasoning"),
        "the thought part must carry the reasoning text: {model_turn}"
    );
    assert_eq!(
        parts[0].get("thought"),
        Some(&serde_json::json!(true)),
        "the surviving part must be a thought part: {model_turn}"
    );
}

/// A tool result that parses to JSON `null` (the upstream omitted the
/// response object) must NOT be emitted as `functionResponse.response: null`.
#[test]
fn test_write_request_null_tool_result_coerced_to_struct() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "get_weather".to_string(),
                // A literal "null" payload — valid JSON that parses to Value::Null.
                content: vec![crate::codec::ir::IrBlock::text("null".to_string())],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let response = wire
        .get("contents")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("parts"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("functionResponse"))
        .and_then(|fr| fr.get("response"))
        .expect("functionResponse.response must exist");
    assert!(
        response.is_object(),
        "a null tool-result payload must be coerced to a Struct (object), got: {response}"
    );
    assert!(
        !response.is_null(),
        "functionResponse.response must never be null: {wire}"
    );
}

/// A bare-scalar tool result ("42") must likewise be coerced to a Struct — wrapped under
/// `{"output": <value>}` so the content survives — never emitted as a raw scalar.
#[test]
fn test_write_request_scalar_tool_result_coerced_to_struct() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "compute".to_string(),
                content: vec![crate::codec::ir::IrBlock::text("42".to_string())],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let response = wire
        .get("contents")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("parts"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("functionResponse"))
        .and_then(|fr| fr.get("response"))
        .expect("functionResponse.response must exist");
    assert!(
        response.is_object(),
        "a scalar tool-result payload must be coerced to a Struct, got: {response}"
    );
    assert_eq!(
        response.get("output").and_then(|v| v.as_i64()),
        Some(42),
        "the scalar value must survive under the `output` key: {response}"
    );
}

/// Regression: Gemini's `Content.role` is OPTIONAL. A single-turn request that omits `role`
#[test]
fn test_read_request_absent_role_defaults_to_user() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"parts": [{"text": "hi"}]}]
    });
    let ir = reader
        .read_request(&body)
        .expect("a role-less content must be accepted as a user turn, not rejected");
    assert_eq!(ir.messages.len(), 1, "one message expected: {ir:?}");
    assert_eq!(
        ir.messages[0].role,
        crate::codec::ir::IrRole::User,
        "an absent role must default to user: {ir:?}"
    );
}

/// An explicitly EMPTY role string (`"role": ""`) is likewise treated as a user turn, matching
/// the absent-role case and the streaming reader's `role_val.is_empty()` leniency.
#[test]
fn test_read_request_empty_role_defaults_to_user() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"role": "", "parts": [{"text": "hi"}]}]
    });
    let ir = reader
        .read_request(&body)
        .expect("an empty role must be accepted as a user turn, not rejected");
    assert_eq!(
        ir.messages[0].role,
        crate::codec::ir::IrRole::User,
        "an empty role must default to user: {ir:?}"
    );
}

/// A genuinely unexpected NON-EMPTY role string is still a hard client error — the leniency is
/// scoped to the absent/empty case (the real API's optional-role default), not to arbitrary
/// role values.
#[test]
fn test_read_request_unknown_nonempty_role_still_rejected() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"role": "function", "parts": [{"text": "hi"}]}]
    });
    assert!(
        reader.read_request(&body).is_err(),
        "an unexpected non-empty role must still be rejected"
    );
}

/// Regression: `model` is preserved in `extra` exactly once (no duplicate insert) and survives
/// the read path because it is excluded from the loop via `modeled_keys`.
#[test]
fn test_read_request_model_preserved_in_extra_once() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "model": "gemini-1.5-pro"
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert_eq!(
        ir.extra.get("model"),
        Some(&serde_json::json!("gemini-1.5-pro")),
        "model must be preserved in extra: {:?}",
        ir.extra
    );
}

/// Regression: a ToolResult whose content is multi-part PLAIN TEXT (not JSON) must be wrapped in
/// `{"output": <text>}` rather than silently discarded as an empty `{}` object.
#[test]
fn test_write_request_tool_result_plaintext_wrapped_not_dropped() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "get_weather".to_string(),
                content: vec![
                    crate::codec::ir::IrBlock::text("sunny".to_string()),
                    crate::codec::ir::IrBlock::text("and warm".to_string()),
                ],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let resp = wire
        .pointer("/contents/0/parts/0/functionResponse/response")
        .expect("functionResponse.response must be present");
    assert_ne!(
        resp,
        &serde_json::json!({}),
        "plain-text tool result must not be discarded as empty object: {wire}"
    );
    assert_eq!(
        resp.get("output").and_then(|o| o.as_str()),
        Some("sunny and warm"),
        "plain-text tool result must be wrapped as {{\"output\": text}}: {wire}"
    );
}

/// A ToolResult whose joined text IS valid JSON is forwarded structurally (not wrapped).
#[test]
fn test_write_request_tool_result_json_passthrough() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "f".to_string(),
                content: vec![crate::codec::ir::IrBlock::text("{\"temp\":21}".to_string())],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let temp = wire
        .pointer("/contents/0/parts/0/functionResponse/response/temp")
        .and_then(|v| v.as_i64());
    assert_eq!(temp, Some(21), "JSON tool result must pass through: {wire}");
}

/// Regression: a cross-protocol response with NO foreign id but a populated `created` (the
/// boundary signal `proxy engine` leaves intact after stripping `id`) must SYNTHESIZE a
/// Gemini-shaped `responseId` so a native SDK always sees a value.
#[test]
fn test_response_identity_cross_protocol_synthesizes_id_when_created_set() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        created: Some(1_700_000_000),
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    let synth = wire
        .get(FIELD_RESPONSE_ID)
        .and_then(|v| v.as_str())
        .expect("cross-protocol response (created set, id none) must synthesize responseId");
    assert!(
        !synth.is_empty(),
        "synthesized responseId must be non-empty: {wire}"
    );
}

/// Regression: the in-stream Error frame emits the FULL google.rpc.Status envelope
/// (`code` int + UPPER_SNAKE `status` + message), not a message-only object, so a Gemini SDK
/// can branch on `error.status`/`error.code`.
#[test]
fn test_stream_error_emits_full_google_rpc_status() {
    let writer = GeminiWriter;
    let err = crate::codec::protocol::IrError {
        class: StatusClass::RateLimit,
        provider_signal: Some("slow down".to_string()),
        retry_after: None,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &IrStreamEvent::Error(err))
        .expect("Error event must emit a frame");
    assert_eq!(
        frame.pointer("/error/code"),
        Some(&serde_json::json!(429)),
        "frame: {frame}"
    );
    assert_eq!(
        frame.pointer("/error/status").and_then(|s| s.as_str()),
        Some("RESOURCE_EXHAUSTED"), // golden wire-contract literal (kept bare on purpose)
        "frame: {frame}"
    );
    assert_eq!(
        frame.pointer("/error/message").and_then(|m| m.as_str()),
        Some("slow down"),
        "frame: {frame}"
    );
}

/// A server-error class maps to 500/INTERNAL in the stream error envelope.
#[test]
fn test_stream_error_server_error_maps_internal() {
    let writer = GeminiWriter;
    let err = crate::codec::protocol::IrError {
        class: StatusClass::ServerError,
        provider_signal: None,
        retry_after: None,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &IrStreamEvent::Error(err))
        .expect("Error event must emit a frame");
    assert_eq!(frame.pointer("/error/code"), Some(&serde_json::json!(500)));
    assert_eq!(
        frame.pointer("/error/status").and_then(|s| s.as_str()),
        Some("INTERNAL") // golden wire-contract literal (kept bare on purpose)
    );

    assert_eq!(
        frame.pointer("/error/message").and_then(|m| m.as_str()),
        Some("error")
    );
}

/// A cross-protocol stream that carries only a model (no id) surfaces `modelVersion` AND a
/// synthesized `responseId` on the leading frame — a native SDK reads both `chunk.model_version`
/// and `chunk.response_id` off the first chunk.
#[test]
fn test_stream_message_start_model_only_emits_model_version() {
    let writer = GeminiWriter;
    let frame = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: Some("gemini-1.5-pro".to_string()),
            },
        )
        .expect("a model-bearing MessageStart must emit a frame");
    assert_eq!(
        frame.1[FIELD_MODEL_VERSION],
        serde_json::json!("gemini-1.5-pro"),
        "frame must carry modelVersion: {}",
        frame.1
    );
    assert!(
        frame
            .1
            .get(FIELD_RESPONSE_ID)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "no id → responseId synthesized so the SDK still sees one: {}",
        frame.1
    );
}

/// Regression: a Gemini `functionCall` in `read_request` must produce a NON-EMPTY tool-use id
/// (Gemini carries none).
#[test]
fn test_read_request_functioncall_gets_nonempty_id() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{
            "role": "model",
            "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let id = ir.messages[0].content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
        _ => None,
    });
    let id = id.expect("ToolUse block must be present");
    assert!(!id.is_empty(), "synthesized tool-use id must be non-empty");
}

/// Regression: two `functionCall`s sharing the SAME function name in one request must get
/// DISTINCT non-empty ids (the call index disambiguates) so `tool_result` routing cannot
/// collapse them on cross-protocol egress.
#[test]
fn test_read_request_same_name_tool_calls_get_distinct_ids() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{
            "role": "model",
            "parts": [
                {"functionCall": {"name": "search", "args": {"q": "a"}}},
                {"functionCall": {"name": "search", "args": {"q": "b"}}}
            ]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let ids: Vec<String> = ir.messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2, "two tool-use blocks expected");
    assert!(ids.iter().all(|i| !i.is_empty()), "ids must be non-empty");
    assert_ne!(
        ids[0], ids[1],
        "repeated function name must still yield distinct ids: {ids:?}"
    );
}

/// Regression: the synthesized id is DETERMINISTIC for a given (index, name) — two reads of the
/// same request body produce the same ids (stable within a request lifetime).
#[test]
fn test_read_request_tool_call_id_is_deterministic() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{
            "role": "model",
            "parts": [{"functionCall": {"name": "f", "args": {}}}]
        }]
    });
    let id_of = |r: &GeminiReader| {
        r.read_request(&body).unwrap().messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(id_of(&reader), id_of(&reader), "id must be deterministic");
}

/// Regression: the same-protocol ToolResult correlation key stays the function NAME (the writer
/// round-trips it into `functionResponse.name`), NOT the synthetic ToolUse id.
#[test]
fn test_read_request_functionresponse_tool_use_id_is_name() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{"functionResponse": {"name": "get_weather", "response": {"t": 21}}}]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let tid = ir.messages[0].content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
        _ => None,
    });
    assert_eq!(
        tid.as_deref(),
        Some("get_weather"),
        "result correlation key must remain the function name for same-protocol round-trip"
    );
}

/// Regression: a `functionCall` in `read_response` (non-stream) must also carry a non-empty id.
#[test]
fn test_read_response_functioncall_gets_nonempty_id() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "f", "args": {}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let id = ir.content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
        _ => None,
    });
    assert!(
        id.is_some_and(|i| !i.is_empty()),
        "response ToolUse must carry a non-empty id"
    );
}

/// A SAFETY-filtered Gemini candidate carries only
/// `finishReason` + `safetyRatings` and NO `content` field.
#[test]
fn test_read_response_safety_filtered_candidate_no_content_is_ok() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "finishReason": GEMINI_FINISH_SAFETY,
            "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH"}]
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 0}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("safety-filtered candidate (no content) must decode, not error");
    assert!(
        ir.content.is_empty(),
        "filtered candidate has no content blocks, got {:?}",
        ir.content
    );
    assert!(
        ir.stop_reason.is_some(),
        "the SAFETY finishReason must still map to a stop_reason"
    );
}

/// A PROMPT-blocked Gemini stream chunk carries a top-level
/// `promptFeedback.blockReason`, NO `candidates`, and NO `error`.
#[test]
fn test_stream_prompt_block_emits_terminal_sequence() {
    let events = collect_stream(&[serde_json::json!({
        "promptFeedback": {"blockReason": GEMINI_FINISH_SAFETY},
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 0}
    })]);

    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::MessageStart { .. }))
            .count(),
        1,
        "prompt-block stream must emit exactly one MessageStart: {events:?}"
    );
    let stop_reason = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
        _ => None,
    });
    assert_eq!(
        stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "prompt-block must surface a `safety` stop_reason: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(IrStreamEvent::MessageStop)),
        "the stream must terminate with MessageStop: {events:?}"
    );

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStart { .. })),
        "a blocked prompt must not open any content block: {events:?}"
    );
}

/// A prompt-level RECITATION block must classify as `safety` (matching the candidate-level
/// mapping and RECITATION's own doc), not `Other`.
#[test]
fn test_stream_prompt_block_recitation_maps_to_safety() {
    let events = collect_stream(&[serde_json::json!({
        "promptFeedback": {"blockReason": GEMINI_FINISH_RECITATION},
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 0}
    })]);
    let stop_reason = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
        _ => None,
    });
    assert_eq!(
        stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "a prompt-level RECITATION block must map to Safety, not Other: {events:?}"
    );
}

/// A MID-STREAM prompt-block arm must close any blocks opened by
/// earlier chunks before emitting its terminal MessageDelta/MessageStop.
#[test]
fn test_stream_mid_stream_prompt_block_closes_open_text_block() {
    let reader = GeminiReader;
    let mut state = StreamDecodeState::default();

    let first = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": "hello"}], "role": "model"}
            }]
        }),
        &mut state,
    );
    assert!(
        first
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStart { index: 0, .. })),
        "first chunk must open block 0: {first:?}"
    );
    assert!(
        state.text_block_open,
        "text block must remain open after the first chunk: {first:?}"
    );

    let second = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "promptFeedback": {"blockReason": GEMINI_FINISH_SAFETY},
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 0}
        }),
        &mut state,
    );

    let stop_reason = second.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
        _ => None,
    });
    assert!(
        matches!(
            second.as_slice(),
            [
                IrStreamEvent::BlockStop { index: 0 },
                IrStreamEvent::MessageDelta { .. },
                IrStreamEvent::MessageStop,
            ]
        ),
        "mid-stream prompt-block must emit [BlockStop{{0}}, MessageDelta, MessageStop]: {second:?}"
    );
    assert_eq!(
        stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "the terminal MessageDelta must carry a `safety` stop_reason: {second:?}"
    );
    assert!(
        !state.text_block_open,
        "the open text block flag must be cleared after the prompt-block close: {second:?}"
    );
}

/// A PROMPT-blocked NON-STREAMING Gemini body (top-level
/// `promptFeedback.blockReason`, NO `candidates`, NO `error`) must decode to an empty-content
/// response with a `safety` stop reason, NOT hard-fail with `ir_parse` (which the old
/// absent-candidates arm did → a spurious client error with no surfaced reason).
#[test]
fn test_read_response_prompt_block_is_safety_stop_not_error() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "promptFeedback": {
            "blockReason": GEMINI_FINISH_PROHIBITED_CONTENT,
            "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH"}]
        },
        "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 0}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("prompt-blocked body must decode, not error");
    assert!(
        ir.content.is_empty(),
        "a blocked prompt has no content blocks, got {:?}",
        ir.content
    );
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "a blocked prompt must surface a `safety` stop_reason"
    );
    assert_eq!(ir.usage.input_tokens, 9, "usage must still be surfaced");
}

/// Regression: a candidates-absent body with NEITHER an error NOR a promptFeedback.blockReason is
/// still a malformed envelope and MUST hard-fail — the prompt-block arm must not swallow it.
#[test]
fn test_read_response_candidates_absent_without_block_still_errors() {
    let reader = GeminiReader;
    let body = serde_json::json!({"usageMetadata": {"promptTokenCount": 1}});
    assert!(
        reader
            .read_response(crate::codec::test_host::HOST, &body)
            .is_err(),
        "a candidates-absent body with no block reason must still error"
    );
}

/// A STREAMING zero-arg `functionCall` (no `args` field) must emit an
/// empty JSON OBJECT `{}` as its InputJsonDelta, NOT `null`.
#[test]
fn test_stream_zero_arg_function_call_emits_empty_object_not_null() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"functionCall": {"name": "ping"}}]},
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);
    let args_json = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            delta: IrDelta::InputJsonDelta(s),
            ..
        } => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        args_json.as_deref(),
        Some("{}"),
        "zero-arg streamed functionCall must serialize to `{{}}`, not `null`: {events:?}"
    );
}

/// A NON-STREAMING zero-arg `functionCall` (no `args` field) must decode
/// to an empty-object `input` (`{}`), NOT `null`.
#[test]
fn test_read_response_zero_arg_function_call_input_is_empty_object_not_null() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"functionCall": {"name": "ping"}}]},
            "finishReason": GEMINI_FINISH_STOP
        }]
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let input = ir.content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::ToolUse { input, .. } => Some(input.clone()),
        _ => None,
    });
    assert_eq!(
        input,
        Some(serde_json::Value::Object(serde_json::Map::new())),
        "zero-arg functionCall input must be `{{}}`, not null: {:?}",
        ir.content
    );
}

/// Regression: the streaming `BlockStart` for a tool block must carry a non-empty synthesized
/// id (Gemini streams carry none) so the Anthropic/OpenAI stream writers emit a usable id.
#[test]
fn test_stream_tool_blockstart_id_is_nonempty() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "f", "args": {}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);
    let id = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            block: IrBlockMeta::ToolUse { id, .. },
            ..
        } => Some(id.clone()),
        _ => None,
    });
    assert!(
        id.is_some_and(|i| !i.is_empty()),
        "stream tool BlockStart must carry a non-empty id"
    );
}

/// A source `stream` is captured into the typed `IrRequest.stream` (used only by path selection)
/// AND preserved in `extra` for byte-identical round-trip (exactly like `model`), so the writer
/// echoes it back.
#[test]
fn test_read_request_source_stream_round_trips_via_extra() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "stream": true
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert!(ir.stream, "stream must be captured into IrRequest.stream");
    assert_eq!(
        ir.extra.get("stream"),
        Some(&serde_json::json!(true)),
        "a source `stream` is preserved in extra for round-trip identity (like model): {:?}",
        ir.extra
    );
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.get("stream"),
        Some(&serde_json::json!(true)),
        "source `stream` round-trips onto the egress body via extra: {wire}"
    );
}

/// A `maxOutputTokens` above `u32::MAX`
#[test]
fn test_read_request_max_output_tokens_overflow_drops_to_none() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"maxOutputTokens": 5_000_000_000i64}
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert_eq!(
        ir.max_tokens, None,
        "an out-of-u32-range maxOutputTokens must drop to None, not truncate"
    );

    let body_ok = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"maxOutputTokens": 1024}
    });
    let ir_ok = reader.read_request(&body_ok).expect("read_request");
    assert_eq!(
        ir_ok.max_tokens,
        Some(1024),
        "in-range cap must be preserved"
    );
}

/// Regression: native Gemini `toolConfig` (camelCase) is NOT in `modeled_keys`, so it
/// round-trips through `extra` and back onto the wire unchanged.
#[test]
fn test_read_request_native_tool_config_round_trips_via_extra() {
    let reader = GeminiReader;
    let tool_config = serde_json::json!({
        "functionCallingConfig": {"mode": "ANY"}
    });
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "toolConfig": tool_config.clone()
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert_eq!(
        ir.extra.get("toolConfig"),
        Some(&tool_config),
        "native toolConfig must round-trip through extra: {:?}",
        ir.extra
    );
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.get("toolConfig"),
        Some(&tool_config),
        "toolConfig must be re-emitted on the wire: {wire}"
    );
}

/// Unmodeled `generationConfig` sub-fields (`responseMimeType` for JSON mode,
/// `thinkingConfig` for extended thinking, `candidateCount`, `seed`, …) MUST survive read→write
/// instead of being silently dropped.
#[test]
fn test_generation_config_unmodeled_subfields_survive_roundtrip() {
    let reader = GeminiReader;
    let writer = GeminiWriter;
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "maxOutputTokens": 256,
            "temperature": 0.3,
            "responseMimeType": MIME_APPLICATION_JSON,
            "thinkingConfig": {"thinkingBudget": 1024},
            "candidateCount": 2,
            "seed": 42
        }
    });

    let ir = reader.read_request(&body).expect("read_request");

    assert!(
        ir.extra.contains_key("generationConfig"),
        "raw generationConfig must be preserved in extra: {:?}",
        ir.extra
    );

    assert_eq!(ir.max_tokens, Some(256));
    assert_eq!(ir.temperature, Some(0.3));

    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    let gc = wire
        .get("generationConfig")
        .and_then(|g| g.as_object())
        .expect("generationConfig must be emitted");

    assert_eq!(gc.get("maxOutputTokens"), Some(&serde_json::json!(256)));
    assert_eq!(gc.get("temperature"), Some(&serde_json::json!(0.3)));

    assert_eq!(
        gc.get(FIELD_RESPONSE_MIME_TYPE),
        Some(&serde_json::json!(MIME_APPLICATION_JSON)),
        "responseMimeType (JSON mode) must survive: {wire}"
    );
    assert_eq!(
        gc.get("thinkingConfig"),
        Some(&serde_json::json!({"thinkingBudget": 1024})),
        "thinkingConfig (extended thinking) must survive: {wire}"
    );
    assert_eq!(gc.get("candidateCount"), Some(&serde_json::json!(2)));
    assert_eq!(gc.get("seed"), Some(&serde_json::json!(42)));

    assert_eq!(
        wire.as_object()
            .map(|o| o.keys().filter(|k| *k == "generationConfig").count()),
        Some(1),
        "generationConfig must appear exactly once: {wire}"
    );
}

/// The typed IR fields OVERLAY the raw extra copy — if the IR's typed
/// `max_tokens` differs from the raw `generationConfig.maxOutputTokens` (e.g. a cross-protocol
/// edit), the typed value wins, mirroring BedrockWriter's inferenceConfig overlay.
#[test]
fn test_generation_config_typed_fields_override_raw_extra() {
    let writer = GeminiWriter;
    let mut extra = serde_json::Map::new();
    extra.insert(
        "generationConfig".to_string(),
        serde_json::json!({"maxOutputTokens": 100, "responseMimeType": "text/plain"}),
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
    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    let gc = wire
        .get("generationConfig")
        .and_then(|g| g.as_object())
        .expect("generationConfig must be emitted");
    assert_eq!(
        gc.get("maxOutputTokens"),
        Some(&serde_json::json!(999)),
        "typed max_tokens must overlay the raw extra value: {wire}"
    );
    assert_eq!(
        gc.get(FIELD_RESPONSE_MIME_TYPE),
        Some(&serde_json::json!("text/plain")),
        "unmodeled sub-field must survive the overlay: {wire}"
    );
}

/// Regression: the streamed `MessageDelta` usage frame must include `totalTokenCount`
#[test]
fn test_stream_message_delta_includes_total_token_count() {
    let writer = GeminiWriter;
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 7,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("MessageDelta must emit a frame");
    assert_eq!(
        frame.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(12)),
        "streamed usage must carry totalTokenCount = prompt + candidates: {frame}"
    );
    assert_eq!(
        frame.pointer("/usageMetadata/promptTokenCount"),
        Some(&serde_json::json!(7))
    );
    assert_eq!(
        frame.pointer("/usageMetadata/candidatesTokenCount"),
        Some(&serde_json::json!(5))
    );
}

/// The streamed total saturates (never overflow-panics) on pathological counts — guards the
/// `saturating_add` on the request path.
#[test]
fn test_stream_message_delta_total_token_count_saturates() {
    let writer = GeminiWriter;
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: u64::MAX,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("MessageDelta must emit a frame");
    assert_eq!(
        frame.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(u64::MAX)),
        "totalTokenCount must saturate, not wrap/panic: {frame}"
    );
}

/// On the CROSS-protocol egress path (signalled by a populated `created` — the
/// boundary marker a non-Gemini backend reader leaves, since Gemini bodies carry no timestamp)
/// the whole-body `write_response` usageMetadata MUST include `totalTokenCount` (= prompt +
/// candidates).
#[test]
fn test_write_response_includes_total_token_count_cross_protocol() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
        created: Some(1_700_000_000),

        system_fingerprint: None,
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(8)),
        "cross-protocol usage must carry totalTokenCount = prompt + candidates: {wire}"
    );
    assert_eq!(
        wire.pointer("/usageMetadata/promptTokenCount"),
        Some(&serde_json::json!(5))
    );
    assert_eq!(
        wire.pointer("/usageMetadata/candidatesTokenCount"),
        Some(&serde_json::json!(3))
    );
}

/// Fidelity guard: a SAME-protocol read→write (no `created` — native Gemini bodies carry no
/// timestamp) must NOT inject `totalTokenCount` the upstream omitted, so the round-trip stays
/// byte-identical. (Production same-protocol passthrough bypasses the writer entirely; this
/// guards the in-IR read→write identity invariant `test_gemini_read_write_response_roundtrip`
/// in mod.rs depends on.)
#[test]
fn test_write_response_omits_total_token_count_same_protocol() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },

        system_fingerprint: None,
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert!(
        wire.pointer("/usageMetadata/totalTokenCount").is_none(),
        "same-protocol round-trip must omit totalTokenCount for byte-identity: {wire}"
    );
}

/// The cross-protocol whole-body total saturates (never overflow-panics) on pathological counts.
#[test]
fn test_write_response_total_token_count_saturates() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: u64::MAX,
            output_tokens: 1,
            ..Default::default()
        },
        created: Some(1_700_000_000),

        system_fingerprint: None,
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(u64::MAX)),
        "totalTokenCount must saturate, not wrap/panic: {wire}"
    );
}

/// A cross-protocol response from a backend whose reader sets `created: None`
#[test]
fn test_write_response_includes_total_token_count_when_only_model_present() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 11,
            output_tokens: 4,
            ..Default::default()
        },

        model: Some("claude-opus-4-8".to_string()),
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(15)),
        "model-only cross-protocol boundary must still carry totalTokenCount: {wire}"
    );
    assert_eq!(
        wire.pointer("/usageMetadata/promptTokenCount"),
        Some(&serde_json::json!(11))
    );
    assert_eq!(
        wire.pointer("/usageMetadata/candidatesTokenCount"),
        Some(&serde_json::json!(4))
    );
}

/// The model-only cross-protocol total saturates (never overflow-panics) on pathological counts,
/// guarding the `saturating_add` on this newly-reachable branch of the request path.
#[test]
fn test_write_response_model_only_total_token_count_saturates() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: u64::MAX,
            output_tokens: 7,
            ..Default::default()
        },
        model: Some("command-r".to_string()),
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/usageMetadata/totalTokenCount"),
        Some(&serde_json::json!(u64::MAX)),
        "totalTokenCount must saturate, not wrap/panic: {wire}"
    );
}

/// The modelled keys are the mapping file's top-level rows, so unmodelled keys still flow to
/// `extra`.
#[test]
fn test_modeled_request_keys_are_the_mapping_rows() {
    let a = |k: &str| crate::codec::carry::models(super::map::REQUEST, k);
    for k in [
        "contents",
        "tools",
        "systemInstruction",
        "model",
        super::GEMINI_JSON_ARRAY_SHIM_KEY,
    ] {
        assert!(a(k), "modeled key set must contain {k}");
    }

    assert!(!a("toolConfig"), "toolConfig must not be modeled");

    assert!(
        !a("generationConfig"),
        "generationConfig must NOT be modeled-out of extra (raw object is preserved for overlay)"
    );
}

/// Hoisting the set must not change read behavior — an unmodeled top-level key
/// still round-trips through `extra`, and the modeled `model` key is preserved exactly once.
#[test]
fn test_read_request_unmodeled_key_still_flows_to_extra_after_hoist() {
    let reader = GeminiReader;
    let j = serde_json::json!({
        "model": "gemini-pro",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}
    });
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    assert_eq!(
        ir.extra.get("toolConfig"),
        Some(&serde_json::json!({"functionCallingConfig": {"mode": "AUTO"}})),
        "unmodeled toolConfig must be preserved in extra"
    );
    assert_eq!(
        ir.extra.get("model"),
        Some(&serde_json::json!("gemini-pro")),
        "modeled `model` is preserved in extra exactly once for round-trip identity"
    );
    assert!(
        !ir.extra.contains_key("contents"),
        "modeled `contents` must NOT leak into extra"
    );
}

/// Regression: a buffered `write_response` with stop_reason=tool_use (the canonical value every
/// other protocol's reader emits for a tool-call turn) must emit finishReason "STOP", NOT the
/// invalid "TOOL_USE" the old upper-casing fallback produced.
#[test]
fn test_write_response_tool_use_maps_to_stop() {
    let writer = GeminiWriter;
    let ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            thought_signature: None,
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "SF"}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/candidates/0/finishReason")
            .and_then(|f| f.as_str()),
        Some("STOP"), // golden wire-contract literal (kept bare on purpose)
        "tool_use must map to STOP, never TOOL_USE: {wire}"
    );
}

/// Regression: the streamed `MessageDelta` with stop_reason=tool_use also emits finishReason
/// "STOP" (matching native Gemini, whose FinishReason enum has no TOOL_USE member).
#[test]
fn test_stream_message_delta_tool_use_maps_to_stop() {
    let writer = GeminiWriter;
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (_, frame) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("MessageDelta must emit a frame");
    assert_eq!(
        frame
            .pointer("/candidates/0/finishReason")
            .and_then(|f| f.as_str()),
        Some("STOP"), // golden wire-contract literal (kept bare on purpose)
        "streamed tool_use must map to STOP, never TOOL_USE: {frame}"
    );
}

/// Regression: an IR Image carrying the `"image_url"` media_type SENTINEL (a remote https URL
/// stored verbatim by the OpenAI/Responses readers) must be emitted as Gemini `fileData{fileUri}`
/// — the native URL reference — NOT as `inlineData` with the URL stuffed into the base64 `data`
/// field and a bogus `mimeType: "image_url"`.
#[test]
fn test_write_request_image_url_sentinel_emits_file_data() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Url(
                    "https://example.com/cat.png".to_string(),
                ),
                cache_control: None,
                detail: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        wire.pointer("/contents/0/parts/0/fileData/fileUri")
            .and_then(|u| u.as_str()),
        Some("https://example.com/cat.png"),
        "image_url sentinel must emit native fileData.fileUri: {wire}"
    );
    assert!(
        wire.pointer("/contents/0/parts/0/inlineData").is_none(),
        "sentinel URL must NOT be emitted as base64 inlineData: {wire}"
    );
}

/// A real base64 image (a genuine mimeType) still emits as `inlineData` — the sentinel branch
/// must not divert legitimate base64 payloads.
#[test]
fn test_write_request_base64_image_still_inline_data() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
                cache_control: None,
                detail: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        wire.pointer("/contents/0/parts/0/inlineData/mimeType")
            .and_then(|m| m.as_str()),
        Some("image/png"),
        "base64 image must stay inlineData: {wire}"
    );
    assert_eq!(
        wire.pointer("/contents/0/parts/0/inlineData/data")
            .and_then(|d| d.as_str()),
        Some("aGVsbG8="),
    );
}

/// Round-trip: a native Gemini `fileData{fileUri}` part reads into the typed `Url` image source
/// and the writer re-emits it as `fileData{fileUri}` verbatim (same-protocol fidelity).
#[test]
fn test_file_data_image_round_trips_via_url() {
    let reader = GeminiReader;
    let writer = GeminiWriter;
    let body = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{"fileData": {"fileUri": "gs://bucket/img.jpg"}}]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    let url = ir.messages[0].content.iter().find_map(|b| match b {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Url(url),
            ..
        } => Some(url.clone()),
        _ => None,
    });
    assert_eq!(
        url.as_deref(),
        Some("gs://bucket/img.jpg"),
        "fileData must read into the typed Url source: {ir:?}"
    );
    let wire = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        wire.pointer("/contents/0/parts/0/fileData/fileUri")
            .and_then(|u| u.as_str()),
        Some("gs://bucket/img.jpg"),
        "fileData must round-trip verbatim: {wire}"
    );
}

/// A synthesized `responseId` must be a native-shaped opaque
/// token — mixed-case alphanumeric base62 of native length, with NO hyphen separator and NO
/// lowercase-hex-only restriction.
#[test]
fn test_synth_response_id_is_opaque_native_shape() {
    let id = synth_response_id(crate::codec::test_host::HOST).expect("test entropy");
    assert_eq!(
        id.len(),
        RESPONSE_ID_TOKEN_LEN,
        "synthesized responseId must be exactly the native token length: {id}"
    );
    assert!(
        !id.contains('-'),
        "synthesized responseId must carry NO hyphen separator (a non-native tell): {id}"
    );
    assert!(
        id.chars().all(|c| c.is_ascii_alphanumeric()),
        "synthesized responseId must be mixed-case alphanumeric (no `-`/`_`): {id}"
    );

    let saw_non_hex = (0..64)
        .map(|_| synth_response_id(crate::codec::test_host::HOST).expect("test entropy"))
        .any(|s| {
            s.chars()
                .any(|c| !c.is_ascii_hexdigit() || c.is_ascii_uppercase())
        });
    assert!(
        saw_non_hex,
        "synthesized responseIds must draw from mixed-case base62, not lowercase-hex only"
    );
}

/// The synthesized id must embed NO unix-second prefix: the old form's leading segment WAS the
/// server clock.
#[test]
fn test_synth_response_id_leaks_no_timestamp() {
    let now_hex = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    for _ in 0..16 {
        let id = synth_response_id(crate::codec::test_host::HOST).expect("test entropy");
        assert!(
            !id.starts_with(&now_hex),
            "synthesized responseId must not lead with the unix-second hex (clock leak): {id}"
        );
        assert!(
            !id.contains('-'),
            "synthesized responseId must not be a hyphenated time-counter pair: {id}"
        );
    }
}

/// The base62 reduction must be UNBIASED. The old body mapped each
/// random byte with a bare `byte % 62`; because `256 % 62 != 0`, the 8 symbols reachable from the
/// partial final block (bytes `248..=255` → residues `0..=7`) were drawn at 5/256 while the other
/// 54 symbols were drawn at 4/256 — a ~25% over-representation of those 8 symbols.
#[test]
fn test_synth_response_id_base62_is_unbiased() {
    use std::collections::HashMap;

    let hot: Vec<char> = RESPONSE_ID_ALPHABET[..8]
        .iter()
        .map(|&b| b as char)
        .collect();

    let mut counts: HashMap<char, u64> = HashMap::new();
    let mut total: u64 = 0;
    for _ in 0..40_000 {
        for c in synth_response_id(crate::codec::test_host::HOST)
            .expect("test entropy")
            .chars()
        {
            *counts.entry(c).or_insert(0) += 1;
            total += 1;
        }
    }
    assert!(total > 0, "burst produced no symbols");

    let hot_sum: u64 = hot.iter().map(|c| *counts.get(c).unwrap_or(&0)).sum();
    let cold_sum: u64 = counts
        .iter()
        .filter(|(c, _)| !hot.contains(c))
        .map(|(_, n)| *n)
        .sum();

    let hot_mean = hot_sum as f64 / hot.len() as f64;
    let cold_count = 62 - hot.len();
    let cold_mean = cold_sum as f64 / cold_count as f64;
    assert!(cold_mean > 0.0, "no cold-symbol samples observed");
    let ratio = hot_mean / cold_mean;

    assert!(
        (0.90..=1.10).contains(&ratio),
        "base62 reduction is biased: hot/cold per-symbol frequency ratio {ratio:.4} \
             (hot_mean {hot_mean:.1}, cold_mean {cold_mean:.1}) — expected ~1.0 from unbiased \
             rejection sampling, ~1.25 indicates the old `byte % 62` bias regressed"
    );
}

/// Uniqueness burst: a large run of synthesized ids must be collision-free in practice (each
/// is ~95 bits).
#[test]
fn test_synth_response_id_uniqueness_burst() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for _ in 0..50_000 {
        let id = synth_response_id(crate::codec::test_host::HOST).expect("test entropy");
        assert_eq!(id.len(), RESPONSE_ID_TOKEN_LEN, "non-native length: {id}");
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric()),
            "non-base62 char in id: {id}"
        );
        assert!(
            seen.insert(id.clone()),
            "synthesized responseId collided: {id}"
        );
    }
}

/// `state.open_tools` is only drained on a `finishReason` chunk,
/// so an upstream that streams an unbounded run of `functionCall` parts WITHOUT a finishReason
/// must not grow it without bound.
#[test]
fn test_stream_open_tools_growth_is_capped() {
    let reader = GeminiReader;
    let mut state = StreamDecodeState::default();

    for n in 0..(MAX_GEMINI_TOOL_FRAMES + 200) {
        reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"functionCall": {"name": format!("f{n}"), "args": {}}}]
                    }
                }]
            }),
            &mut state,
        );
    }
    assert!(
        state.open_tools.len() <= MAX_GEMINI_TOOL_FRAMES,
        "open_tools must be capped at MAX_GEMINI_TOOL_FRAMES, got {}",
        state.open_tools.len()
    );
}

/// Past `MAX_GEMINI_TOOL_FRAMES` the drop must be OBSERVABLE, not silent: the first frame that
/// hits the cap latches `state.tool_frame_cap_warned` (which also gates a `tracing::warn!`
/// with the cap value).
#[test]
fn test_stream_tool_frame_cap_drop_is_observable() {
    let reader = GeminiReader;
    let mut state = StreamDecodeState::default();

    for n in 0..10 {
        reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"functionCall": {"name": format!("f{n}"), "args": {}}}]
                    }
                }]
            }),
            &mut state,
        );
    }
    assert!(
        !state.tool_frame_cap_warned,
        "cap-drop latch must not fire while under MAX_GEMINI_TOOL_FRAMES"
    );

    for n in 0..(MAX_GEMINI_TOOL_FRAMES + 200) {
        reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"functionCall": {"name": format!("g{n}"), "args": {}}}]
                    }
                }]
            }),
            &mut state,
        );
    }
    assert!(
        state.tool_frame_cap_warned,
        "dropping functionCall frames past MAX_GEMINI_TOOL_FRAMES must be observable via the latch"
    );
}

/// The cap must NOT perturb a realistic stream: a small number of tool calls are all recorded
/// and each gets a matching BlockStart it can close on finishReason.
#[test]
fn test_stream_open_tools_under_cap_records_all() {
    let reader = GeminiReader;
    let mut state = StreamDecodeState::default();
    let mut starts = 0usize;
    for n in 0..3 {
        for ev in reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"functionCall": {"name": format!("f{n}"), "args": {}}}]
                    }
                }]
            }),
            &mut state,
        ) {
            if matches!(ev, IrStreamEvent::BlockStart { .. }) {
                starts += 1;
            }
        }
    }
    assert_eq!(state.open_tools.len(), 3, "all 3 tool frames recorded");
    assert_eq!(starts, 3, "each tool frame emits exactly one BlockStart");
}

/// A well-formed credential yields exactly one `x-goog-api-key` header carrying the verbatim key.
#[test]
fn test_auth_headers_valid_key_emits_x_goog_api_key() {
    let raw = crate::codec::protocol::CredentialHeader::Raw {
        header: "x-goog-api-key",
        trim_start: false,
    };
    let Some(crate::codec::protocol::EgressScheme::Static {
        families,
        own,
        passthrough,
    }) = crate::codec::gemini::DECL.egress_scheme
    else {
        panic!("gemini declares a static credential scheme");
    };
    assert!(families.is_empty());
    assert_eq!(own, raw);
    assert_eq!(passthrough, raw);
    assert!(
        crate::codec::gemini::DECL.egress_auth_headers.is_none(),
        "no credential passes through the plane"
    );
}

/// HIGH/correctness regression: a Tool-role IR message carries `ToolResult` blocks, which the
/// writer emits as Gemini `functionResponse` parts.
#[test]
fn test_tool_role_maps_to_user_for_function_response() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "get_weather".to_string(),
                content: vec![crate::codec::ir::IrBlock::text("{\"temp\":21}".to_string())],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };

    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let content = &wire["contents"][0];

    assert_eq!(
        content["role"], "user",
        "a Tool-role message carrying a functionResponse must be emitted under role:\"user\", \
             never \"model\": {wire}"
    );

    let fr = &content["parts"][0]["functionResponse"];
    assert_eq!(
        fr["name"], "get_weather",
        "functionResponse must name the tool: {wire}"
    );
    assert_eq!(
        fr["response"]["temp"], 21,
        "structured JSON tool output must be forwarded verbatim: {wire}"
    );
}

/// HIGH/correctness regression: an Assistant-role message carrying a `functionCall` (ToolUse)
/// must still be emitted under `role:"model"` — the fix to the Tool role must NOT regress the
/// assistant mapping, since `functionCall`s are exclusively a model-turn shape.
#[test]
fn test_assistant_tool_use_stays_model_role() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({ "city": "SF" }),
                cache_control: None,
            }],
        }],
        ..Default::default()
    };

    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let content = &wire["contents"][0];
    assert_eq!(
        content["role"], "model",
        "an Assistant functionCall turn must stay role:\"model\": {wire}"
    );
    assert_eq!(
        content["parts"][0][FIELD_FUNCTION_CALL]["name"], "get_weather",
        "functionCall must be preserved under the model turn: {wire}"
    );
}

/// An inline `{"error":{...}}` google.rpc.Status object
/// delivered as a 200-status SSE data chunk mid-stream MUST surface as a single
/// `IrStreamEvent::Error` (mapped from `error.status`) rather than being silently swallowed.
#[test]
fn test_stream_inline_error_envelope_surfaces_ir_error() {
    let events = collect_stream(&[serde_json::json!({
        "error": {
            "code": 429,
            "message": "Resource has been exhausted (e.g. check quota).",
            "status": GRPC_RESOURCE_EXHAUSTED
        }
    })]);

    match events.as_slice() {
        [IrStreamEvent::Error(err)] => {
            assert_eq!(err.class, StatusClass::RateLimit, "events: {events:?}");
            assert_eq!(
                err.provider_signal.as_deref(),
                Some("Resource has been exhausted (e.g. check quota)."),
                "events: {events:?}"
            );
        }
        other => panic!("expected exactly one IrStreamEvent::Error, got {other:?}"),
    }
}

/// An inline error whose `status` is absent falls back to the numeric `code` mapping (503 →
/// Overloaded), and a missing/unknown code defaults to ServerError — never silently dropped.
#[test]
fn test_stream_inline_error_code_fallback_and_default() {
    let by_code = collect_stream(&[serde_json::json!({
        "error": { "code": 503, "message": "backend overloaded" }
    })]);
    match by_code.as_slice() {
        [IrStreamEvent::Error(err)] => {
            assert_eq!(err.class, StatusClass::Overloaded, "events: {by_code:?}")
        }
        other => panic!("expected one Error, got {other:?}"),
    }

    let bare = collect_stream(&[serde_json::json!({
        "error": { "message": "something failed" }
    })]);
    match bare.as_slice() {
        [IrStreamEvent::Error(err)] => {
            assert_eq!(err.class, StatusClass::ServerError, "events: {bare:?}");
            assert_eq!(
                err.provider_signal.as_deref(),
                Some("something failed"),
                "events: {bare:?}"
            );
        }
        other => panic!("expected one Error, got {other:?}"),
    }
}

/// `gemini_error_status_class` prefers the UPPER_SNAKE `status` over the numeric `code`, and an
/// unrecognized status string falls through to the code mapping.
#[test]
fn test_gemini_error_status_class_mapping() {
    assert_eq!(
        gemini_error_status_class(Some(GRPC_UNAVAILABLE), Some(503)),
        StatusClass::Overloaded
    );
    assert_eq!(
        gemini_error_status_class(Some(GRPC_UNAUTHENTICATED), Some(401)),
        StatusClass::Auth
    );
    assert_eq!(
        gemini_error_status_class(Some(GRPC_PERMISSION_DENIED), Some(403)),
        StatusClass::Billing
    );
    assert_eq!(
        gemini_error_status_class(Some(GRPC_DEADLINE_EXCEEDED), Some(504)),
        StatusClass::Timeout
    );
    assert_eq!(
        gemini_error_status_class(Some(GRPC_INVALID_ARGUMENT), Some(400)),
        StatusClass::ClientError
    );

    assert_eq!(
        gemini_error_status_class(Some(GRPC_INTERNAL), Some(429)),
        StatusClass::ServerError
    );

    assert_eq!(
        gemini_error_status_class(Some("SOME_FUTURE_CODE"), Some(429)),
        StatusClass::RateLimit
    );
}

/// The Gemini bad-key auth-failure envelope MUST carry the
/// canonical `error.details[]` array with a google.rpc.ErrorInfo whose `reason` is
/// `API_KEY_INVALID`.
#[test]
fn test_write_error_bad_key_carries_api_key_invalid_details() {
    let writer = GeminiWriter;
    let envelope = writer.write_error(
        crate::codec::test_host::HOST,
        400,
        "invalid_request_error",
        super::GEMINI_BAD_KEY_MESSAGE,
    );

    assert_eq!(
        envelope.pointer("/error/code"),
        Some(&serde_json::json!(400)),
        "envelope: {envelope}"
    );
    assert_eq!(
        envelope.pointer("/error/status").and_then(|s| s.as_str()),
        Some("INVALID_ARGUMENT"), // golden wire-contract literal (kept bare on purpose)
        "envelope: {envelope}"
    );
    let detail = envelope
        .pointer("/error/details/0")
        .expect("bad-key envelope must carry error.details[0]");
    assert_eq!(
        detail.get("@type").and_then(|t| t.as_str()),
        Some("type.googleapis.com/google.rpc.ErrorInfo"), // golden wire-contract literal (kept bare on purpose)
        "detail: {detail}"
    );
    assert_eq!(
        detail.get("reason").and_then(|r| r.as_str()),
        Some("API_KEY_INVALID"), // golden wire-contract literal (kept bare on purpose)
        "detail: {detail}"
    );
    assert_eq!(
        detail.get("domain").and_then(|d| d.as_str()),
        Some("googleapis.com"),
        "detail: {detail}"
    );
    assert_eq!(
        detail.pointer("/metadata/service").and_then(|s| s.as_str()),
        Some("generativelanguage.googleapis.com"),
        "detail: {detail}"
    );
}

/// A NON-auth 400/INVALID_ARGUMENT (e.g. a generic malformed-request body) must NOT grow the
/// API_KEY_INVALID details array — real Google does not carry that reason on a non-key 400, so
/// over-filling it would itself be a tell.
#[test]
fn test_write_error_generic_invalid_argument_has_no_details() {
    let writer = GeminiWriter;
    let envelope = writer.write_error(
        crate::codec::test_host::HOST,
        400,
        "invalid_request_error",
        "Invalid value at 'contents'.",
    );
    assert_eq!(
        envelope.pointer("/error/status").and_then(|s| s.as_str()),
        Some("INVALID_ARGUMENT"), // golden wire-contract literal (kept bare on purpose)
        "envelope: {envelope}"
    );
    assert!(
        envelope.pointer("/error/details").is_none(),
        "a non-bad-key 400 must NOT carry API_KEY_INVALID details: {envelope}"
    );
}

/// HIGH/correctness regression: on a CROSS-protocol multi-turn tool call (Anthropic/OpenAI
/// ingress → Gemini egress) the IR's ToolUse carries a SYNTHETIC `call_<hash>` id and the matching
/// ToolResult's `tool_use_id` carries that SAME synthetic id — NOT the real function name.
#[test]
fn test_write_request_cross_protocol_function_response_name_matches_call() {
    let writer = GeminiWriter;
    let synthetic_id = "call_00000000deadbeef".to_string();
    let req = crate::codec::ir::IrRequest {
        messages: vec![
            // Assistant turn: the tool CALL carries a synthetic id, real name `get_weather`.
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: synthetic_id.clone(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({ "city": "SF" }),
                    cache_control: None,
                }],
            },
            // Tool turn: the RESULT references the call by the SAME synthetic id (cross-protocol
            // seam keeps the id, not the name).
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: synthetic_id.clone(),
                    content: vec![crate::codec::ir::IrBlock::text("{\"temp\":21}".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        ..Default::default()
    };

    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let call_name = wire
        .pointer("/contents/0/parts/0/functionCall/name")
        .and_then(|n| n.as_str());
    let resp_name = wire
        .pointer("/contents/1/parts/0/functionResponse/name")
        .and_then(|n| n.as_str());
    assert_eq!(
        call_name,
        Some("get_weather"),
        "functionCall.name must be the real tool name: {wire}"
    );
    assert_eq!(
        resp_name,
        Some("get_weather"),
        "functionResponse.name must resolve to the real function name (matching \
             functionCall.name), NOT the synthetic call_<hash> id: {wire}"
    );
    assert_eq!(
        call_name, resp_name,
        "Gemini correlates by name: functionResponse.name MUST equal functionCall.name: {wire}"
    );
}

/// Same-protocol (Gemini→Gemini) regression guard for the fix above: when the ToolResult's
/// `tool_use_id` is ALREADY the function name (the reader's same-protocol behavior) and there is no
/// matching ToolUse id in the request, the writer must FALL BACK to that name verbatim — the
/// id→name map lookup must not blank it out.
#[test]
fn test_write_request_same_protocol_function_response_name_falls_back_to_id() {
    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "get_weather".to_string(),
                content: vec![crate::codec::ir::IrBlock::text("{\"temp\":21}".to_string())],
                is_error: false,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        wire.pointer("/contents/0/parts/0/functionResponse/name")
            .and_then(|n| n.as_str()),
        Some("get_weather"),
        "same-protocol functionResponse.name must fall back to the tool_use_id (the name): {wire}"
    );
}

/// LOW/completeness regression: a STREAMING chunk with an EMPTY `candidates: []` array (rather than
/// an absent array) alongside a top-level `promptFeedback.blockReason` must still route into the
/// prompt-block terminal arm — MessageStart, then a `safety` MessageDelta + MessageStop.
#[test]
fn test_stream_empty_candidates_array_prompt_block_terminates() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [],
        "promptFeedback": {"blockReason": GEMINI_FINISH_SAFETY},
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 0}
    })]);
    let stop_reason = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
        _ => None,
    });
    assert_eq!(
        stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "an empty candidates[] + blockReason stream must surface a `safety` stop: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(IrStreamEvent::MessageStop)),
        "the empty-candidates prompt-block stream must terminate with MessageStop: {events:?}"
    );
}

/// LOW/completeness regression: a NON-STREAMING body with an EMPTY `candidates: []` array plus a
/// top-level `promptFeedback.blockReason` must decode to an empty-content `safety` response, NOT
/// hard-fail `candidates.is_empty()` into a spurious `ir_parse` error (the old behavior, since
/// `candidates_absent` treated `[]` as present and skipped the prompt-block arm).
#[test]
fn test_read_response_empty_candidates_array_prompt_block_is_safety_stop() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [],
        "promptFeedback": {"blockReason": GEMINI_FINISH_PROHIBITED_CONTENT},
        "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 0}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("empty-candidates prompt-blocked body must decode, not error");
    assert!(
        ir.content.is_empty(),
        "a blocked prompt has no content blocks, got {:?}",
        ir.content
    );
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety),
        "an empty candidates[] + blockReason body must surface a `safety` stop_reason"
    );
}

/// Guard: an EMPTY `candidates: []` with NO block reason and NO error is still a malformed
/// envelope and MUST hard-fail (the broadened `candidates_absent` routes it into the prompt-block
/// arm, which finds no reason and falls through to the existing empty-array hard-fail below).
#[test]
fn test_read_response_empty_candidates_array_without_block_still_errors() {
    let reader = GeminiReader;
    let body = serde_json::json!({"candidates": [], "usageMetadata": {"promptTokenCount": 1}});
    assert!(
        reader
            .read_response(crate::codec::test_host::HOST, &body)
            .is_err(),
        "an empty candidates[] body with no block reason must still error"
    );
}

/// Gemini has no TOOL_USE finishReason — a tool-call turn ends with STOP.
#[test]
fn test_read_response_stop_with_function_call_is_tool_use() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("tool-call STOP body must decode");
    assert!(
        ir.content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })),
        "the response must carry a ToolUse block: {:?}",
        ir.content
    );
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse),
        "STOP + functionCall must read back as `tool_use`, not `end_turn`"
    );
}

/// Companion guard: a plain STOP with NO tool block must stay `end_turn`. The promotion
/// must be gated on a ToolUse block being present, not applied to every STOP.
#[test]
fn test_read_response_plain_stop_stays_end_turn() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hello"}]},
            "finishReason": GEMINI_FINISH_STOP
        }]
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("plain STOP must decode");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn),
        "a plain STOP with no tool block must stay `end_turn`"
    );
}

/// The streaming reader mapped STOP → `end_turn` on the
/// terminal MessageDelta unconditionally, leaking `end_turn` for a tool-call turn on the streamed
/// cross-protocol path.
#[test]
fn test_stream_stop_with_function_call_terminal_is_tool_use() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);
    let stop = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(
        stop.flatten(),
        Some(crate::codec::ir::IrStopReason::ToolUse),
        "streamed STOP + functionCall must terminate with `tool_use`: {events:?}"
    );
}

/// Companion guard (streaming): a plain STOP stream with no tool block must terminate
/// with `end_turn` (no spurious promotion when `state.open_tools` is empty).
#[test]
fn test_stream_plain_stop_terminal_stays_end_turn() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hello"}]},
            "finishReason": GEMINI_FINISH_STOP
        }]
    })]);
    let stop = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(
        stop.flatten(),
        Some(crate::codec::ir::IrStopReason::EndTurn),
        "a plain STOP stream must terminate with `end_turn`: {events:?}"
    );
}

/// A `ToolUse.input` that is a JSON ARRAY must be coerced to a valid Gemini
/// `functionCall.args` OBJECT (Gemini `args` is a protobuf Struct).
#[test]
fn test_tool_use_array_input_coerced_to_object_args() {
    let block = crate::codec::ir::IrBlock::ToolUse {
        thought_signature: None,
        id: "call_1".to_string(),
        name: "do_thing".to_string(),
        input: serde_json::json!([1, 2, 3]),
        cache_control: None,
    };

    let writer = GeminiWriter;
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![block.clone()],
        }],
        ..Default::default()
    };
    let wire = writer.write_request(crate::codec::test_host::HOST, &req);
    let args = wire
        .pointer("/contents/0/parts/0/functionCall/args")
        .expect("functionCall.args must be present");
    assert!(
        args.is_object(),
        "request functionCall.args MUST be an object (Gemini Struct), got: {args}"
    );
    assert_eq!(
        args.pointer("/args"),
        Some(&serde_json::json!([1, 2, 3])),
        "array input must be wrapped under `args`: {args}"
    );

    let resp = crate::codec::ir::IrResponse {
        content: vec![block],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        ..Default::default()
    };
    let rwire = writer.write_response(crate::codec::test_host::HOST, &resp);
    let rargs = rwire
        .pointer("/candidates/0/content/parts/0/functionCall/args")
        .expect("response functionCall.args must be present");
    assert!(
        rargs.is_object(),
        "response functionCall.args MUST be an object, got: {rargs}"
    );
    assert_eq!(
        rargs.pointer("/args"),
        Some(&serde_json::json!([1, 2, 3])),
        "array input must be wrapped under `args` in the response too: {rargs}"
    );
}

/// Companion guard: an OBJECT `ToolUse.input` must pass through byte-identical so the
/// same-protocol Gemini→Gemini round-trip stays lossless (no `{"args": ...}` wrapping).
#[test]
fn test_tool_use_object_input_passes_through_unchanged() {
    let writer = GeminiWriter;
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            thought_signature: None,
            id: "call_1".to_string(),
            name: "do_thing".to_string(),
            input: serde_json::json!({"city": "SF", "unit": "C"}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        ..Default::default()
    };
    let rwire = writer.write_response(crate::codec::test_host::HOST, &resp);
    let rargs = rwire
        .pointer("/candidates/0/content/parts/0/functionCall/args")
        .expect("functionCall.args must be present");
    assert_eq!(
        rargs,
        &serde_json::json!({"city": "SF", "unit": "C"}),
        "object input must pass through unchanged (no `args` wrapper): {rargs}"
    );
}

fn gemini_read(body: serde_json::Value) -> crate::codec::ir::IrRequest {
    GeminiReader
        .read_request(&body)
        .expect("gemini read_request")
}

#[test]
fn tool_choice_any_required_roundtrips() {
    let ir = gemini_read(serde_json::json!({
        "contents": [],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY"}}
    }));
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );
    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["toolConfig"]["functionCallingConfig"],
        serde_json::json!({"mode": "ANY"})
    );
}

#[test]
fn tool_choice_specific_tool_roundtrips() {
    let ir = gemini_read(serde_json::json!({
        "contents": [],
        "toolConfig": {"functionCallingConfig":
            {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}}
    }));
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string()
        })
    );
    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["toolConfig"]["functionCallingConfig"],
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["get_weather"]})
    );
}

/// Fidelity: `ANY` + `allowedFunctionNames` with N>1 names cannot be expressed by the
/// IR's single-tool `Tool` variant.
#[test]
fn tool_choice_multi_name_allowlist_relaxes_to_required() {
    let ir = gemini_read(serde_json::json!({
        "contents": [],
        "toolConfig": {"functionCallingConfig":
            {"mode": "ANY", "allowedFunctionNames": ["get_weather", "get_time"]}}
    }));
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required),
        "a multi-name allow-list must relax to Required, not Tool{{name: first}}"
    );

    let ir_one = gemini_read(serde_json::json!({
        "contents": [],
        "toolConfig": {"functionCallingConfig":
            {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}}
    }));
    assert_eq!(
        ir_one.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string()
        })
    );
}

#[test]
fn tool_choice_none_and_auto_roundtrip() {
    for (mode, variant) in [
        ("AUTO", crate::codec::ir::IrToolChoice::Auto),
        ("NONE", crate::codec::ir::IrToolChoice::None),
    ] {
        let ir = gemini_read(serde_json::json!({
            "contents": [],
            "toolConfig": {"functionCallingConfig": {"mode": mode}}
        }));
        assert_eq!(ir.tool_choice, Some(variant));
        let writer = GeminiWriter;
        let out = writer.write_request(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], mode);
    }
}

#[test]
fn tool_choice_absent_emits_no_function_calling_config() {
    let ir = gemini_read(serde_json::json!({"contents": []}));
    assert_eq!(ir.tool_choice, None);
    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("toolConfig")
            .and_then(|tc| tc.get("functionCallingConfig"))
            .is_none(),
        "absent tool_choice must NOT synthesize a functionCallingConfig"
    );
}

#[test]
fn tool_choice_no_duplicate_function_calling_config() {
    let ir = gemini_read(serde_json::json!({
        "contents": [],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY"}}
    }));
    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let s = serde_json::to_string(&out).unwrap();
    assert_eq!(
        s.matches("functionCallingConfig").count(),
        1,
        "exactly one functionCallingConfig must appear, got: {s}"
    );
}

/// The raw `toolConfig` the reader preserves in `req.extra` (for
/// same-protocol byte-identity) must NOT clobber the freshly-overlaid `functionCallingConfig` the
/// writer builds from the TYPED `req.tool_choice`.
#[test]
fn tool_choice_overlay_survives_stale_raw_toolconfig_in_extra() {
    let mut req = base_ir_request();
    req.tools.push(crate::codec::ir::IrTool {
        name: "get_weather".to_string(),
        description: None,
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
        cache_control: None,
        hosted: None,
        strict: None,
    });

    req.tool_choice = Some(crate::codec::ir::IrToolChoice::Tool {
        name: "get_weather".to_string(),
    });

    req.extra.insert(
        "toolConfig".to_string(),
        serde_json::json!({"functionCallingConfig": {"mode": "AUTO"}}),
    );

    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out["toolConfig"]["functionCallingConfig"],
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["get_weather"]}),
        "the typed tool_choice overlay must win over a stale raw toolConfig preserved in extra; \
         got {out}"
    );
}

/// `synth_tool_call_id` (used by `read_response`, one call per Gemini
/// response/turn) hashes only `(call_index, function_name)`.
#[test]
fn response_tool_call_ids_do_not_collide_across_conversation_turns() {
    let reader = GeminiReader;
    let turn1 = serde_json::json!({
        "responseId": "resp-turn-1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "NYC"}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
    });
    let turn2 = serde_json::json!({
        "responseId": "resp-turn-2",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "LA"}}}]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
    });

    let id1 = reader
        .read_response(crate::codec::test_host::HOST, &turn1)
        .expect("read_response turn1")
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("turn1 tool_use id");
    let id2 = reader
        .read_response(crate::codec::test_host::HOST, &turn2)
        .expect("read_response turn2")
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("turn2 tool_use id");

    assert_ne!(
        id1, id2,
        "two different conversation turns' first same-named tool call must NOT synthesize the \
         same tool_use id (got {id1} for both)"
    );
}

#[test]
fn finish_reason_maps_gemini_only_reasons_to_canonical() {
    use crate::codec::ir::IrStopReason as S;

    assert_eq!(
        map_gemini_finish_reason(GEMINI_FINISH_RECITATION),
        S::Safety
    );
    assert_eq!(map_gemini_finish_reason("IMAGE_SAFETY"), S::Safety);
    assert_eq!(map_gemini_finish_reason("SPII"), S::Safety);

    assert_eq!(
        map_gemini_finish_reason(GEMINI_FINISH_MALFORMED_FUNCTION_CALL),
        S::Error
    );

    assert_eq!(map_gemini_finish_reason(GEMINI_FINISH_OTHER), S::Other);
    assert_eq!(map_gemini_finish_reason("LANGUAGE"), S::Other);

    assert_eq!(map_gemini_finish_reason(GEMINI_FINISH_STOP), S::EndTurn);
    assert_eq!(
        map_gemini_finish_reason(GEMINI_FINISH_MAX_TOKENS),
        S::MaxTokens
    );
    assert_eq!(map_gemini_finish_reason(GEMINI_FINISH_SAFETY), S::Safety);
}

#[test]
fn finish_reason_recitation_in_response_is_safety() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"parts": [{"text": "x"}], "role": "model"},
            "finishReason": GEMINI_FINISH_RECITATION
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    });
    let resp = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety)
    );
}

/// End-to-end through read_response: a MALFORMED_FUNCTION_CALL finishReason surfaces as the
/// canonical `error` stop_reason (a failed generation with no runnable tool call) — NOT `tool_use`.
#[test]
fn finish_reason_malformed_function_call_in_response_is_error() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"parts": [{"text": "x"}], "role": "model"},
            "finishReason": GEMINI_FINISH_MALFORMED_FUNCTION_CALL
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    });
    let resp = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Error)
    );
}

/// The STREAMING reader (read_response_events) routes finishReason through the SAME
/// `map_gemini_finish_reason`, so a MALFORMED_FUNCTION_CALL terminal chunk must surface
/// stop_reason=Error on the MessageDelta — guarding the stream call site, not just the unit fn.
#[test]
fn finish_reason_malformed_function_call_stream_is_error() {
    let chunks = vec![serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "x"}]},
            "finishReason": GEMINI_FINISH_MALFORMED_FUNCTION_CALL
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    })];
    let events = collect_stream(&chunks);
    let stop = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(
        stop,
        Some(Some(crate::codec::ir::IrStopReason::Error)),
        "streamed MALFORMED_FUNCTION_CALL must terminate stop_reason=Error; got {events:?}"
    );
    let err_at = events
        .iter()
        .position(|e| matches!(e, IrStreamEvent::Error(_)))
        .unwrap_or_else(|| {
            panic!("streamed MALFORMED_FUNCTION_CALL must push an Error event; got {events:?}")
        });
    let IrStreamEvent::Error(err) = &events[err_at] else {
        unreachable!()
    };
    assert_eq!(
        err.class,
        busbar_contract::upstream::StatusClass::ServerError,
        "a failed generation is a transient server fault"
    );
    assert_eq!(
        err.provider_signal.as_deref(),
        Some(GEMINI_FINISH_MALFORMED_FUNCTION_CALL)
    );
    let delta_at = events
        .iter()
        .position(|e| matches!(e, IrStreamEvent::MessageDelta { .. }))
        .expect("the MessageDelta still rides along");
    assert!(
        err_at < delta_at,
        "the Error precedes the terminal MessageDelta"
    );
    assert!(
        matches!(events.last(), Some(IrStreamEvent::MessageStop)),
        "the stream stays properly terminated; got {events:?}"
    );
    let usage = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { usage, .. } => Some(usage.clone()),
        _ => None,
    });
    assert_eq!(
        usage.map(|u| (u.input_tokens, u.output_tokens)),
        Some((1, 1)),
        "the streamed usage still rides the MessageDelta (the charge, #62)"
    );
}

/// `raw_stop_reason` reads the FIRST candidate's `finishReason` off the RAW bytes and maps it
/// exactly as `read_response` does (owner ruling Q31 follow-up: the same-protocol relay's breaker
/// fault), with bounded work after locating the key.
#[test]
fn raw_stop_reason_agrees_with_read_response() {
    for token in [
        GEMINI_FINISH_MALFORMED_FUNCTION_CALL,
        "STOP",
        "MAX_TOKENS",
        "SAFETY",
        "OTHER",
    ] {
        let body = serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "x"}]},
                "finishReason": token
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
        });
        let parsed = GeminiReader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read_response");
        assert_eq!(
            crate::codec::test_host::raw_stop_reason(&GeminiReader, body.to_string().as_bytes()),
            parsed.stop_reason,
            "{token}"
        );
    }

    let long = format!(
        "{{\"candidates\":[{{\"finishReason\":\"{}\"}}]}}",
        "A".repeat(4096)
    );
    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&GeminiReader, long.as_bytes()),
        None
    );
    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&GeminiReader, b"{}"),
        None
    );
}

/// The relay's INCREMENTAL scan answers the same wherever the body is split — the key, the colon,
/// the quotes or the token cut at any byte (owner ruling Q31 follow-up: the ungoverned
/// same-protocol relay keeps no copy of the body) — and it reads only a candidate's own
/// `finishReason` member: the name spelled inside delivered text, or as a key of a model-written
/// tool `args` object, is content.
#[test]
fn stop_key_scanner_reads_the_candidate_member_at_every_split() {
    use crate::codec::usage_tail::StopKeyScanner;
    let bodies: [(&[u8], Option<&str>); 6] = [
        (
            br#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason" : "MALFORMED_FUNCTION_CALL"}]}"#,
            Some("MALFORMED_FUNCTION_CALL"),
        ),
        (
            br#"{"candidates":[{"content":{"parts":[{"text":"\"finishReason\":\"STOP\""}]},"finishReason":"SAFETY"}]}"#,
            Some("SAFETY"),
        ),
        (br#"{"candidates":[{"finishReason":null}]}"#, None),
        (br#"{"usageMetadata":{"promptTokenCount":1}}"#, None),
        (br#"{"candidates":[{"finishReason":"has\"escape"}]}"#, None),
        (
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"finishReason":"MALFORMED_FUNCTION_CALL"}}}]},"finishReason":"STOP"}]}"#,
            Some("STOP"),
        ),
    ];
    let key = GeminiReader.stop_reason_key().expect("gemini is scanned");
    for (body, want) in bodies {
        for a in 0..=body.len() {
            for b in a..=body.len() {
                let mut scan = StopKeyScanner::new(key).expect("a short key");
                scan.feed(&body[..a]);
                scan.feed(&body[a..b]);
                scan.feed(&body[b..]);
                assert_eq!(
                    scan.token(),
                    want,
                    "split at {a}/{b} of {}",
                    String::from_utf8_lossy(body)
                );
            }
        }
    }

    let long = format!(
        "{{\"candidates\":[{{\"finishReason\":\"{}\"}}]}}",
        "A".repeat(200)
    );
    let mut scan = StopKeyScanner::new(key).expect("a short key");
    for byte in long.as_bytes().chunks(1) {
        scan.feed(byte);
    }
    assert_eq!(scan.token(), None);
}

/// A MODEL-WRITTEN TOOL ARGUMENT NEVER FAULTS THE LANE. A Gemini answer writes `content` before
/// `finishReason`, so a `functionCall.args` object holding `"finishReason":"MALFORMED_FUNCTION_CALL"`
/// was the FIRST `finishReason` in the body: the same-protocol relay read it as a failed generation
/// and raised a transient fault on a lane other tenants share.
#[test]
fn a_finish_reason_inside_tool_args_is_not_the_stop_reason() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"functionCall": {
                "name": "report",
                "args": {"finishReason": "MALFORMED_FUNCTION_CALL"}
            }}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
    });

    assert_eq!(
        crate::codec::test_host::raw_stop_reason(&GeminiReader, body.to_string().as_bytes()),
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
    let parsed = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        parsed.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
    assert!(!crate::exchange::reply::wire::generation_failed(
        crate::codec::test_host::HOST,
        "gemini",
        &body
    ));
}

/// The Error event is scoped to the failed-generation reason: a SAFETY stop is a correctly-served
/// refusal and a MAX_TOKENS stop a truncation, neither a lane fault.
#[test]
fn finish_reason_non_error_stops_push_no_error_event() {
    for reason in ["STOP", "MAX_TOKENS", "SAFETY", "OTHER"] {
        let events = collect_stream(&[serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "x"}]},
                "finishReason": reason
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
        })]);
        assert!(
            !events.iter().any(|e| matches!(e, IrStreamEvent::Error(_))),
            "{reason} must not push an Error event; got {events:?}"
        );
    }
}

#[test]
fn context_length_override_only_fires_on_400_or_413() {
    let token_body =
        br#"{"error":{"code":429,"message":"input is longer than the maximum number of tokens"}}"#;

    let err = GeminiReader.extract_error(StatusCode::TOO_MANY_REQUESTS, token_body);
    assert_ne!(
        err.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 429 with token-phrased body must NOT be mis-dispositioned as ContextLength"
    );

    let body_400 =
        br#"{"error":{"code":400,"message":"input is longer than the maximum number of tokens"}}"#;
    let err = GeminiReader.extract_error(StatusCode::BAD_REQUEST, body_400);
    assert_eq!(
        err.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 400 with the token-overflow message must classify as context_length_exceeded"
    );

    let err = GeminiReader.extract_error(StatusCode::PAYLOAD_TOO_LARGE, token_body);
    assert_eq!(
        err.provider_code.as_deref(),
        Some("context_length_exceeded")
    );
}

/// Minimal IR request with a single user "hi" turn and all controls defaulted. Tests mutate the
/// field(s) under test so the assertion targets exactly one gap.
fn base_ir_request() -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    }
}

/// OpenAI→Gemini: an IR carrying frequency/presence penalties, seed, and n MUST emit them in
/// Gemini's native generationConfig shape (frequencyPenalty/presencePenalty/seed/candidateCount).
#[test]
fn test_write_request_sampling_controls_emit_generation_config() {
    let mut req = base_ir_request();
    req.frequency_penalty = Some(0.5);
    req.presence_penalty = Some(-0.25);
    req.seed = Some(42);
    req.n = Some(3);
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &req)
    };
    let gc = wire
        .get("generationConfig")
        .expect("generationConfig must be emitted");
    assert_eq!(gc.get("frequencyPenalty"), Some(&serde_json::json!(0.5)));
    assert_eq!(gc.get("presencePenalty"), Some(&serde_json::json!(-0.25)));
    assert_eq!(gc.get("seed"), Some(&serde_json::json!(42)));
    assert_eq!(
        gc.get("candidateCount"),
        Some(&serde_json::json!(3)),
        "n maps to Gemini candidateCount: {wire}"
    );
}

/// None sampling controls emit NOTHING (no spurious zero penalties / seed on a plain request).
#[test]
fn test_write_request_sampling_controls_omitted_when_none() {
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &base_ir_request())
    };
    if let Some(gc) = wire.get("generationConfig") {
        assert!(gc.get("frequencyPenalty").is_none());
        assert!(gc.get("presencePenalty").is_none());
        assert!(gc.get("seed").is_none());
        assert!(gc.get("candidateCount").is_none());
    }
}

/// Gemini→IR: native generationConfig sampling controls promote into the typed IR fields, and a
/// read→write round-trips them (same-protocol fidelity).
#[test]
fn test_read_request_sampling_controls_promote_and_round_trip() {
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "frequencyPenalty": 0.7,
            "presencePenalty": 0.1,
            "seed": 99,
            "candidateCount": 2
        }
    });
    let ir = GeminiReader.read_request(&body).expect("read_request");
    assert_eq!(ir.frequency_penalty, Some(0.7));
    assert_eq!(ir.presence_penalty, Some(0.1));
    assert_eq!(ir.seed, Some(99));
    assert_eq!(ir.n, Some(2), "candidateCount promotes to IR n");

    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    let gc = wire.get("generationConfig").expect("generationConfig");
    assert_eq!(gc.get("frequencyPenalty"), Some(&serde_json::json!(0.7)));
    assert_eq!(gc.get("presencePenalty"), Some(&serde_json::json!(0.1)));
    assert_eq!(gc.get("seed"), Some(&serde_json::json!(99)));
    assert_eq!(gc.get("candidateCount"), Some(&serde_json::json!(2)));
}

/// Gemini→IR→Gemini: native responseMimeType + responseSchema read into the normalized IR
/// response_format object and round-trip back into generationConfig.
#[test]
fn test_response_format_round_trips_native_gemini() {
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "responseMimeType": MIME_APPLICATION_JSON,
            "responseSchema": {"type": "object", "properties": {"x": {"type": "string"}}}
        }
    });
    let ir = GeminiReader.read_request(&body).expect("read_request");
    let rf = ir
        .response_format
        .as_ref()
        .expect("response_format must be populated");
    assert!(
        rf.json,
        "application/json mime must read as a JSON directive"
    );
    assert!(rf.schema.is_some(), "responseSchema must be carried");

    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    let gc = wire.get("generationConfig").expect("generationConfig");
    assert_eq!(
        gc.get(FIELD_RESPONSE_MIME_TYPE),
        Some(&serde_json::json!("application/json")) // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(
        gc.pointer("/responseSchema/properties/x/type"),
        Some(&serde_json::json!("string")),
        "responseSchema must round-trip: {wire}"
    );
}

/// A JSON structured-output directive (from any source protocol — here a typed IR carrying a
/// schema) maps onto Gemini responseMimeType + responseSchema, with JSON-Schema keywords Gemini
/// rejects (e.g. `$schema`) stripped by `sanitize_gemini_schema`.
#[test]
fn test_response_format_maps_to_gemini_and_sanitizes_schema() {
    let mut req = base_ir_request();
    req.response_format = Some(crate::codec::ir::IrResponseFormat {
        json: true,
        schema: Some(serde_json::json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#"
        })),
        name: None,
        strict: None,
        description: None,
    });
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &req)
    };
    let gc = wire.get("generationConfig").expect("generationConfig");
    assert_eq!(
        gc.get(FIELD_RESPONSE_MIME_TYPE),
        Some(&serde_json::json!("application/json")), // golden wire-contract literal (kept bare on purpose)
        "json_schema type maps to application/json: {wire}"
    );
    assert_eq!(
        gc.pointer("/responseSchema/type"),
        Some(&serde_json::json!("object"))
    );
    assert!(
        gc.pointer("/responseSchema/$schema").is_none(),
        "rejected JSON-Schema keyword $schema must be stripped from responseSchema: {wire}"
    );
}

/// A Gemini response `thought:true` part with a `thoughtSignature` reads into IrBlock::Thinking
/// (text + signature), and write_response re-emits it as a `{text, thought:true,
/// thoughtSignature}` part — full round-trip with the signature preserved.
#[test]
fn test_thought_part_round_trips_through_ir_thinking() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "let me reason", "thought": true, "thoughtSignature": "sig-abc"},
                {"text": "the answer"}
            ]},
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    });
    let resp = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    match &resp.content[0] {
        crate::codec::ir::IrBlock::Thinking {
            text, signature, ..
        } => {
            assert_eq!(text, "let me reason");
            assert_eq!(signature.as_deref(), Some("sig-abc"));
        }
        other => panic!("expected Thinking block, got {other:?}"),
    }
    assert!(matches!(
        &resp.content[1],
        crate::codec::ir::IrBlock::Text { text, .. } if text == "the answer"
    ));

    let wire = {
        let __w = GeminiWriter;
        __w.write_response(crate::codec::test_host::HOST, &resp)
    };
    let part0 = wire
        .pointer("/candidates/0/content/parts/0")
        .expect("first part");
    assert_eq!(part0.get("text"), Some(&serde_json::json!("let me reason")));
    assert_eq!(part0.get("thought"), Some(&serde_json::json!(true)));
    assert_eq!(
        part0.get("thoughtSignature"),
        Some(&serde_json::json!("sig-abc")),
        "thoughtSignature must round-trip: {wire}"
    );
}

/// A Thinking block in a REQUEST assistant turn round-trips through write_request as a thought
/// part with its signature (cross-protocol reasoning survives into a Gemini request).
#[test]
fn test_thinking_block_round_trips_in_write_request() {
    let mut req = base_ir_request();
    req.messages.push(crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::Thinking {
            text: "thinking...".to_string(),
            signature: Some("sig-1".to_string()),
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin: None,
        }],
    });
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &req)
    };

    let parts = wire
        .pointer("/contents/1/parts")
        .and_then(|p| p.as_array())
        .expect("model parts");
    let thought = &parts[0];
    assert_eq!(thought.get("text"), Some(&serde_json::json!("thinking...")));
    assert_eq!(thought.get("thought"), Some(&serde_json::json!(true)));
    assert_eq!(
        thought.get("thoughtSignature"),
        Some(&serde_json::json!("sig-1")),
        "request-side thought signature must round-trip: {wire}"
    );
}

/// Gemini usageMetadata.cachedContentTokenCount maps into the IR cache_read_input_tokens field
/// (the same field Bedrock/Anthropic cache-read map to), surviving the cross-protocol seam.
#[test]
fn test_cached_content_token_count_reads_into_cache_read() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hi"}]},
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 5,
            "cachedContentTokenCount": 80
        }
    });
    let resp = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.usage.cache_read_input_tokens,
        Some(80),
        "cachedContentTokenCount must map to cache_read_input_tokens"
    );

    let body_no_cache = serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": GEMINI_FINISH_STOP}],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    });
    let resp2 = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body_no_cache)
        .expect("read_response");
    assert_eq!(resp2.usage.cache_read_input_tokens, None);
}

/// A Gemini `fileData` part (with or without a mimeType) is a remote URL reference → reads into
/// the typed `Url` source and re-emits as `fileData{fileUri}`.
#[test]
fn test_file_data_reads_into_url_source_and_round_trips() {
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [
            {"fileData": {"fileUri": "gs://bucket/img.png", "mimeType": "image/png"}}
        ]}]
    });
    let ir = GeminiReader.read_request(&body).expect("read_request");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::UrlWithMime { url, .. },
            ..
        } => assert_eq!(url, "gs://bucket/img.png"),
        other => panic!("expected Image(Url), got {other:?}"),
    }
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    assert_eq!(
        wire.pointer("/contents/0/parts/0/fileData/fileUri"),
        Some(&serde_json::json!("gs://bucket/img.png")),
        "fileUri must round-trip as a fileData reference: {wire}"
    );
}

/// Regression guard: a fileData WITHOUT a mimeType (bare remote URI) reads back as an image_url
/// sentinel and re-emits as `fileData{fileUri, mimeType}`.
#[test]
fn test_file_data_url_reemits_with_derived_mime_type() {
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"fileData": {"fileUri": "https://x/i.jpg"}}]}]
    });
    let ir = GeminiReader.read_request(&body).expect("read_request");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Url(url),
            ..
        } => {
            assert_eq!(url, "https://x/i.jpg");
        }
        other => panic!("expected Image, got {other:?}"),
    }
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &ir)
    };
    assert_eq!(
        wire.pointer("/contents/0/parts/0/fileData/fileUri"),
        Some(&serde_json::json!("https://x/i.jpg"))
    );
    assert_eq!(
        wire.pointer("/contents/0/parts/0/fileData/mimeType"),
        Some(&serde_json::json!("image/jpeg")),
        "fileData must carry a mimeType derived from the URL extension: {wire}"
    );
}

/// A cross-protocol tool def carrying JSON-Schema keywords Gemini 400-rejects ($schema,
/// additionalProperties, $ref, …) must be stripped on write so the tool def survives instead of
/// hard-failing — recursively, including nested object/array schemas.
#[test]
fn test_write_request_strips_rejected_schema_keywords() {
    let mut req = base_ir_request();
    req.tools.push(crate::codec::ir::IrTool {
        name: "get_weather".to_string(),
        description: Some("w".to_string()),
        input_schema: serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "loc": {"type": "string"},
                "nested": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {"$ref": {"type": "string"}}
                }
            },
            "required": ["loc"]
        }),
        cache_control: None,

        hosted: None,

        strict: None,
    });
    let wire = {
        let __w = GeminiWriter;
        __w.write_request(crate::codec::test_host::HOST, &req)
    };
    let params = wire
        .pointer("/tools/0/functionDeclarations/0/parameters")
        .expect("parameters");
    assert!(
        params.get("$schema").is_none(),
        "$schema must be stripped: {wire}"
    );

    assert_eq!(
        params.get("additionalProperties"),
        Some(&serde_json::json!(false)),
        "boolean additionalProperties is accepted by Gemini and must survive: {wire}"
    );

    assert_eq!(params.get("type"), Some(&serde_json::json!("object")));
    assert_eq!(params.get("required"), Some(&serde_json::json!(["loc"])));
    assert_eq!(
        params.pointer("/properties/loc/type"),
        Some(&serde_json::json!("string"))
    );

    assert_eq!(
        params.pointer("/properties/nested/additionalProperties"),
        Some(&serde_json::json!(true)),
        "nested boolean additionalProperties must survive recursively: {wire}"
    );
    assert_eq!(
        params.pointer("/properties/nested/type"),
        Some(&serde_json::json!("object"))
    );
}

/// `sanitize_gemini_schema` leaves a clean Gemini-native schema untouched and walks arrays.
#[test]
fn test_sanitize_gemini_schema_preserves_clean_and_walks_arrays() {
    let clean = serde_json::json!({
        "type": "object",
        "anyOf": [{"type": "string"}, {"type": "number", "$comment": "drop me"}]
    });
    let out = sanitize_gemini_schema(&clean);
    assert_eq!(out.get("type"), Some(&serde_json::json!("object")));
    assert_eq!(
        out.pointer("/anyOf/0/type"),
        Some(&serde_json::json!("string"))
    );
    assert!(
        out.pointer("/anyOf/1/$comment").is_none(),
        "rejected keyword in an array element must be stripped: {out}"
    );
    assert_eq!(
        out.pointer("/anyOf/1/type"),
        Some(&serde_json::json!("number"))
    );
}

/// The keyword filter is POSITIONAL. Inside a `properties` map the keys
/// are FIELD NAMES THE CALLER CHOSE, not JSON-Schema keywords, so a tool schema with a property
/// named `examples` / `const` / `definitions` / `$ref` must survive intact.
#[test]
fn test_sanitize_gemini_schema_keeps_user_properties_named_like_keywords() {
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "examples": {"type": "array", "items": {"type": "string"}, "const": "drop me"},
            "const": {"type": "string"},
            "definitions": {"type": "boolean"},
            "$ref": {"type": "number"},
            "query": {"type": "string", "$comment": "drop me"}
        },
        "required": ["examples", "const", "definitions", "$ref", "query"]
    });
    let out = sanitize_gemini_schema(&schema);

    assert!(
        out.get("$schema").is_none(),
        "a real schema-level keyword must still be stripped: {out}"
    );

    for name in ["examples", "const", "definitions", "$ref", "query"] {
        assert!(
            out.pointer(&format!(
                "/properties/{}",
                name.replace('~', "~0").replace('/', "~1")
            ))
            .is_some(),
            "user property `{name}` was deleted by the keyword filter: {out}"
        );
    }
    assert_eq!(
        out.get("required"),
        schema.get("required"),
        "`required` is never rewritten, so every name in it must still be a property"
    );

    assert!(
        out.pointer("/properties/examples/const").is_none(),
        "a keyword inside a property's own schema must still be stripped: {out}"
    );
    assert!(
        out.pointer("/properties/query/$comment").is_none(),
        "a keyword inside a property's own schema must still be stripped: {out}"
    );
    assert_eq!(
        out.pointer("/properties/examples/items/type"),
        Some(&serde_json::json!("string")),
        "the property's own schema is otherwise preserved verbatim: {out}"
    );
}

/// A realistic Pydantic/Zod-generated nested-model schema: a top-level `address` property is a
/// `$ref` into a `$defs` entry, and that entry is itself an object with its own typed properties
/// and `required` list -- exactly the shape every JSON-Schema-emitting SDK produces for a nested
/// model.
#[test]
fn test_sanitize_gemini_schema_inlines_nested_ref_against_defs() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "address": {"$ref": "#/$defs/Address"}
        },
        "required": ["name", "address"],
        "$defs": {
            "Address": {
                "type": "object",
                "properties": {
                    "street": {"type": "string"},
                    "zip": {"type": "string"}
                },
                "required": ["street", "zip"]
            }
        }
    });
    let out = sanitize_gemini_schema(&resolve_gemini_schema_refs(&schema));

    assert_eq!(
        out.pointer("/properties/address/type"),
        Some(&serde_json::json!("object")),
        "a $ref'd nested model must be inlined into a real typed object, not collapsed to {{}}: {out}"
    );
    assert_eq!(
        out.pointer("/properties/address/properties/street/type"),
        Some(&serde_json::json!("string")),
        "the referenced model's own properties must survive inlining: {out}"
    );
    assert_eq!(
        out.pointer("/properties/address/properties/zip/type"),
        Some(&serde_json::json!("string"))
    );
    assert_eq!(
        out.pointer("/properties/address/required"),
        Some(&serde_json::json!(["street", "zip"])),
        "the referenced model's own `required` list must survive inlining: {out}"
    );

    assert!(out.get("$defs").is_none());
    assert!(out.pointer("/properties/address/$ref").is_none());
}

/// `additionalProperties` is only rejected by the live Gemini API when it carries a SCHEMA (the
/// `dict[str, Model]` shape Pydantic/Zod emit as `{"additionalProperties": {"$ref": ...}}`) --
/// google-gemini/gemini-cli#13694 shows the backend 400ing with "Expected boolean, received
/// object" on exactly that shape.
#[test]
fn test_sanitize_gemini_schema_keeps_boolean_additional_properties_strips_schema_form() {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "tags": {
                "type": "object",
                "additionalProperties": true
            },
            "meta": {
                "type": "object",
                "additionalProperties": {"type": "string"}
            }
        }
    });
    let out = sanitize_gemini_schema(&resolve_gemini_schema_refs(&schema));
    assert_eq!(
        out.get("additionalProperties"),
        Some(&serde_json::json!(false)),
        "boolean additionalProperties is genuinely supported and must survive: {out}"
    );
    assert_eq!(
        out.pointer("/properties/tags/additionalProperties"),
        Some(&serde_json::json!(true))
    );
    assert!(
        out.pointer("/properties/meta/additionalProperties")
            .is_none(),
        "schema-valued additionalProperties is rejected by the live API and must still be stripped: {out}"
    );
}

/// Gemini's THINKING tokens are billed and must be ledgered.
#[test]
fn test_gemini_usage_counts_thinking_tokens_as_output() {
    let with_thoughts = serde_json::json!({
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 11,
            "thoughtsTokenCount": 512,
            "totalTokenCount": 623
        }
    });
    let u = gemini_usage(&with_thoughts);
    assert_eq!(u.input_tokens, 100);
    assert_eq!(
        u.output_tokens, 523,
        "thinking tokens are GENERATED tokens: they belong in output_tokens with the visible answer"
    );

    assert_eq!(
        u.billable_tokens(),
        with_thoughts["usageMetadata"]["totalTokenCount"]
            .as_u64()
            .unwrap(),
        "IR billable total must equal Gemini's own totalTokenCount"
    );

    let no_thoughts = serde_json::json!({
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 11, "totalTokenCount": 111}
    });
    assert_eq!(gemini_usage(&no_thoughts).output_tokens, 11);

    let cached = serde_json::json!({
        "usageMetadata": {
            "promptTokenCount": 100,
            "cachedContentTokenCount": 40,
            "candidatesTokenCount": 11,
            "thoughtsTokenCount": 512
        }
    });
    let u = gemini_usage(&cached);
    assert_eq!(u.input_tokens, 60);
    assert_eq!(u.cache_read_input_tokens, Some(40));
    assert_eq!(u.output_tokens, 523);
}

/// The Gemini stream WRITE path must emit a streamed reasoning part for a `ThinkingDelta`
#[test]
fn test_stream_thinking_and_signature_deltas_emit_thought_parts() {
    let writer = GeminiWriter;

    let think = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::ThinkingDelta("let me reason".to_string()),
            },
        )
        .expect("a ThinkingDelta must emit a streamed thought chunk, not None");
    let think_part = think
        .1
        .pointer("/candidates/0/content/parts/0")
        .expect("thought chunk must carry a part");
    assert_eq!(
        think_part.pointer("/text").and_then(|t| t.as_str()),
        Some("let me reason"),
        "streamed thought part must carry the reasoning text: {think_part}"
    );
    assert_eq!(
        think_part.pointer("/thought"),
        Some(&serde_json::json!(true)),
        "streamed thought part must be flagged thought:true: {think_part}"
    );

    let sig = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::SignatureDelta("sig-xyz".to_string()),
            },
        )
        .expect("a SignatureDelta must emit a streamed thought chunk, not None");
    let sig_part = sig
        .1
        .pointer("/candidates/0/content/parts/0")
        .expect("signature chunk must carry a part");
    assert_eq!(
        sig_part
            .pointer("/thoughtSignature")
            .and_then(|s| s.as_str()),
        Some("sig-xyz"),
        "streamed signature part must carry the thoughtSignature: {sig_part}"
    );
    assert_eq!(
        sig_part.pointer("/thought"),
        Some(&serde_json::json!(true)),
        "streamed signature part must be flagged thought:true: {sig_part}"
    );
}

/// A Gemini response carrying `candidates[].citationMetadata.citationSources[]` must read
/// into IrCitation(s) on the answer's Text block (url/title/indices preserved), and a CROSS-
/// protocol Cohere egress must re-emit citations carrying
/// the url/title — closing the grounding-citation gap that previously dropped them entirely.
#[test]
fn gemini_citation_metadata_reads_and_projects_to_cohere() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "The sky is blue."}]
            },
            "citationMetadata": {
                "citationSources": [
                    {
                        "startIndex": 0,
                        "endIndex": 15,
                        "uri": "https://example.com/sky",
                        "title": "Why the sky is blue",
                        "license": ""
                    }
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 4,
            "totalTokenCount": 9
        }
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    let citations = ir
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } if !citations.is_empty() => {
                Some(citations)
            }
            _ => None,
        })
        .expect("citations must be attached to the Text block");
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].url.as_deref(), Some("https://example.com/sky"));
    assert_eq!(citations[0].title.as_deref(), Some("Why the sky is blue"));
    assert_eq!(citations[0].start_index, Some(0));
    assert_eq!(citations[0].end_index, Some(15));

    let wire = Protocol::cohere()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    let c = wire
        .pointer("/message/citations/0")
        .expect("Cohere citation");
    assert_eq!(c["start"], serde_json::json!(0));
    assert_eq!(c["end"], serde_json::json!(15));
    assert_eq!(
        c["sources"][0]["document"]["url"],
        serde_json::json!("https://example.com/sky")
    );
    assert_eq!(
        c["sources"][0]["document"]["title"],
        serde_json::json!("Why the sky is blue")
    );
}

/// Same-protocol Gemini→IR→Gemini citation round-trip re-emits candidate-level
/// citationMetadata (verbatim source via `raw`), and a response WITHOUT citations stays free of a
/// `citationMetadata` key.
#[test]
fn gemini_citations_roundtrip_and_absent_unaffected() {
    let reader = GeminiReader;
    let writer = GeminiWriter;
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Answer."}]},
            "citationMetadata": {
                "citationSources": [
                    {"startIndex": 1, "endIndex": 7, "uri": "https://src/x", "title": "X"}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
    });
    let mut ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    ir.created = Some(1);
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    let src = wire
        .pointer("/candidates/0/citationMetadata/citationSources/0")
        .expect("citationMetadata re-emitted at candidate level");
    assert_eq!(
        src.get("uri").and_then(|v| v.as_str()),
        Some("https://src/x")
    );
    assert_eq!(src.get("startIndex").and_then(|v| v.as_i64()), Some(1));

    let plain_body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Plain."}]},
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
    });
    let mut plain_ir = reader
        .read_response(crate::codec::test_host::HOST, &plain_body)
        .expect("read_response");
    plain_ir.created = Some(1);
    let plain_wire = writer.write_response(crate::codec::test_host::HOST, &plain_ir);
    assert!(
        plain_wire
            .pointer("/candidates/0/citationMetadata")
            .is_none(),
        "no citationMetadata for a citation-free response; got {plain_wire}"
    );
}

/// A Gemini candidate whose content splits its output across MULTIPLE text
/// parts.
#[test]
fn gemini_citation_offsets_correct_across_multiple_text_parts() {
    let reader = GeminiReader;

    let first_part = "Hello world. ";
    let second_part = "Paris is great.";
    assert_eq!(first_part.len(), 13);
    let body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": first_part}, {"text": second_part}]
            },
            "citationMetadata": {
                "citationSources": [
                    {"startIndex": 13, "endIndex": 18, "uri": "https://example.com/paris", "title": "Paris"}
                ]
            },
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 8}
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    let text_blocks: Vec<(&str, &Vec<crate::codec::ir::IrCitation>)> = ir
        .content
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::Text {
                text, citations, ..
            } => Some((text.as_str(), citations)),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_blocks.len(),
        2,
        "both text parts must survive as blocks"
    );

    assert!(
        text_blocks[0].1.is_empty(),
        "citation must not attach to the first text block when its span lands in the second: {:?}",
        text_blocks[0].1
    );

    assert_eq!(
        text_blocks[1].1.len(),
        1,
        "citation must attach to the second text block, whose span actually contains it"
    );
    let c = &text_blocks[1].1[0];
    assert_eq!(
        c.start_index,
        Some(0),
        "start_index must be re-expressed relative to the owning block's own text"
    );
    assert_eq!(
        c.end_index,
        Some(5),
        "end_index must be re-expressed relative to the owning block's own text"
    );
    assert_eq!(&text_blocks[1].0[0..5], "Paris");
}

/// Gemini's candidate-level `citationSources[]` is CUMULATIVE: each chunk restates every source
/// seen so far.
#[test]
fn stream_gemini_cumulative_citation_sources_emit_only_the_new_ones() {
    let source_a = serde_json::json!({
        "startIndex": 0,
        "endIndex": 5,
        "uri": "https://example.com/a",
        "title": "A"
    });
    let source_b = serde_json::json!({
        "startIndex": 6,
        "endIndex": 11,
        "uri": "https://example.com/b",
        "title": "B"
    });
    let events = collect_stream(&[
        serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "alpha"}]},
                "citationMetadata": {"citationSources": [source_a]}
            }]
        }),
        serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": " beta"}]},
                "citationMetadata": {"citationSources": [source_a, source_b]},
                "finishReason": GEMINI_FINISH_STOP
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 4}
        }),
    ]);

    let deltas: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::CitationsDelta(cs),
                ..
            } => Some(
                cs.iter()
                    .map(|c| c.title.clone().unwrap_or_default())
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![vec!["A".to_string()], vec!["B".to_string()]],
        "each chunk must carry only the citations it ADDED, got {deltas:?}"
    );
}

/// STREAMING citations, cross-protocol: a Gemini stream chunk carrying candidate-level
/// `citationMetadata.citationSources[]` must read into an `IrDelta::CitationsDelta` on the answer
/// text block, and a cross-protocol Cohere egress must re-emit its url/title — closing the streaming
/// grounding-citation gap that previously dropped the citation entirely.
#[test]
fn stream_gemini_citation_metadata_projects_to_cohere_citation_start() {
    let events = collect_stream(&[
        serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "The sky is blue."}]}
            }]
        }),
        serde_json::json!({
            "candidates": [{
                "citationMetadata": {
                    "citationSources": [
                        {
                            "startIndex": 0,
                            "endIndex": 15,
                            "uri": "https://example.com/sky",
                            "title": "Why the sky is blue"
                        }
                    ]
                },
                "finishReason": GEMINI_FINISH_STOP
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 4}
        }),
    ]);

    let (cite_idx, citations) = events
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::CitationsDelta(cs),
            } if !cs.is_empty() => Some((*index, cs.clone())),
            _ => None,
        })
        .expect("a CitationsDelta must be emitted for a streamed Gemini citation");
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].url.as_deref(), Some("https://example.com/sky"));
    assert_eq!(citations[0].title.as_deref(), Some("Why the sky is blue"));
    assert_eq!(citations[0].start_index, Some(0));
    assert_eq!(citations[0].end_index, Some(15));

    assert!(
        events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart { index, .. } if *index == cite_idx
        )),
        "citation block must be opened: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStop { index } if *index == cite_idx
        )),
        "citation block must be closed: {events:?}"
    );

    let protocol = Protocol::cohere();
    let frames: Vec<_> = events
        .iter()
        .flat_map(|event| {
            protocol
                .writer()
                .write_response_events(crate::codec::test_host::HOST, event)
        })
        .collect();
    let citation = frames
        .iter()
        .find_map(|(_, frame)| {
            (frame["type"] == "citation-start").then(|| &frame["delta"]["message"]["citations"])
        })
        .expect("native Cohere citation-start");
    assert_eq!(
        citation["sources"][0]["document"]["url"],
        serde_json::json!("https://example.com/sky")
    );
    assert_eq!(
        citation["sources"][0]["document"]["title"],
        serde_json::json!("Why the sky is blue")
    );
}

/// STREAMING citations, reverse direction: an `IrDelta::CitationsDelta` (as an Anthropic
/// stream reader would produce) must project onto a native Gemini `citationMetadata.citationSources`
/// chunk when written by the Gemini writer — and the shape-gate must REBUILD the Gemini source from
/// the neutral fields rather than leak the foreign Anthropic `raw` through.
#[test]
fn stream_anthropic_citation_projects_to_gemini_citation_metadata() {
    let writer = GeminiWriter;

    writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("0123456789".to_string()),
        },
    );
    let cit = crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".to_string()),
        cited_text: None,
        title: Some("Doc Title".to_string()),
        url: Some("https://anthropic.example/doc".to_string()),
        document_index: None,
        start_index: Some(3),
        end_index: Some(9),
        encrypted_index: Some("enc-xyz".to_string()),
        raw: Some(serde_json::json!({
            "type": "web_search_result_location",
            "url": "https://anthropic.example/doc",
            "title": "Doc Title",
            "encrypted_index": "enc-xyz"
        })),
        ..Default::default()
    };
    let ev = IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::CitationsDelta(vec![cit]),
    };
    let (_, body) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("Gemini writer must emit a citationMetadata chunk for a CitationsDelta");
    let src = body
        .pointer("/candidates/0/citationMetadata/citationSources/0")
        .expect("chunk must carry candidate-level citationSources");

    assert_eq!(
        src.get("uri").and_then(|v| v.as_str()),
        Some("https://anthropic.example/doc")
    );
    assert_eq!(src.get("title").and_then(|v| v.as_str()), Some("Doc Title"));
    assert_eq!(src.get("startIndex").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(src.get("endIndex").and_then(|v| v.as_i64()), Some(9));

    assert!(
        src.get("type").is_none(),
        "Anthropic `raw` must not leak through the Gemini writer: {src}"
    );
}

/// A Gemini stream with NO citations must be unaffected — no `CitationsDelta` is produced,
/// and the block balance is unchanged.
#[test]
fn stream_gemini_no_citations_unaffected() {
    let events = collect_stream(&[serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Plain answer."}]},
            "finishReason": GEMINI_FINISH_STOP
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
    })]);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::CitationsDelta(_),
                ..
            }
        )),
        "a citation-free stream must not produce a CitationsDelta: {events:?}"
    );
    let starts = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
        .count();
    let stops = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStop { .. }))
        .count();
    assert_eq!(starts, stops, "unbalanced block events: {events:?}");
}

/// Each `IrToolChoice` variant must round-trip through Gemini's native
/// `toolConfig.functionCallingConfig` shape (the tool-selection directive is a headline
/// cross-protocol control — ir.rs:82): AUTO↔Auto, NONE↔None, ANY↔Required, and
/// ANY+allowedFunctionNames:[n]↔Tool{n}.
#[test]
fn tool_choice_variants_round_trip_via_function_calling_config() {
    let cases = [
        (crate::codec::ir::IrToolChoice::Auto, "AUTO", None),
        (crate::codec::ir::IrToolChoice::None, "NONE", None),
        (crate::codec::ir::IrToolChoice::Required, "ANY", None),
        (
            crate::codec::ir::IrToolChoice::Tool {
                name: "get_weather".to_string(),
            },
            "ANY",
            Some("get_weather"),
        ),
    ];
    for (variant, expect_mode, expect_name) in cases {
        let wire = write_gemini_tool_choice(&variant);
        assert_eq!(
            wire.pointer("/mode").and_then(|m| m.as_str()),
            Some(expect_mode),
            "mode mismatch for {variant:?}"
        );
        if let Some(name) = expect_name {
            assert_eq!(
                wire.pointer("/allowedFunctionNames/0")
                    .and_then(|n| n.as_str()),
                Some(name)
            );
        } else {
            assert!(
                wire.get("allowedFunctionNames").is_none(),
                "no allowedFunctionNames expected for {variant:?}"
            );
        }

        let tool_config = serde_json::json!({"functionCallingConfig": wire});
        assert_eq!(
            read_gemini_tool_choice(Some(&tool_config)),
            Some(variant.clone()),
            "round-trip failed for {variant:?}"
        );
    }
}

/// An UNKNOWN native `finishReason` (a future/novel Gemini token) maps to `IrStopReason::Other`
#[test]
fn unknown_finish_reason_maps_to_other_and_writes_native_other() {
    assert_eq!(
        map_gemini_finish_reason("SOME_FUTURE_REASON"),
        crate::codec::ir::IrStopReason::Other
    );

    assert_eq!(
        write_gemini_stop_reason(crate::codec::ir::IrStopReason::Other),
        GEMINI_FINISH_OTHER
    );

    assert_eq!(
        map_gemini_finish_reason(GEMINI_FINISH_MALFORMED_FUNCTION_CALL),
        crate::codec::ir::IrStopReason::Error
    );
}

/// cachedContentTokenCount NORMALIZATION, WRITE side (the read side is covered by
/// `test_cached_content_token_count_reads_into_cache_read`): the IR stores UNCACHED input, so the
/// writer must ADD the cached prefix BACK to re-derive Gemini's TOTAL `promptTokenCount` and
/// re-emit `cachedContentTokenCount`.
#[test]
fn write_response_reconstructs_prompt_token_count_with_cached() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 20,
            output_tokens: 5,
            cache_read_input_tokens: Some(80),
            ..Default::default()
        },
        model: Some("gemini-2.0-flash".to_string()),
        ..Default::default()
    };
    let out = {
        let w = GeminiWriter;
        w.write_response(crate::codec::test_host::HOST, &resp)
    };
    let um = out.get("usageMetadata").expect("usageMetadata present");
    assert_eq!(
        um.get(FIELD_PROMPT_TOKEN_COUNT),
        Some(&serde_json::json!(100)),
        "promptTokenCount must re-add cached: uncached(20) + cached(80) = 100"
    );
    assert_eq!(
        um.get(FIELD_CACHED_CONTENT_TOKEN_COUNT),
        Some(&serde_json::json!(80))
    );
    assert_eq!(
        um.get(FIELD_CANDIDATES_TOKEN_COUNT),
        Some(&serde_json::json!(5))
    );

    assert_eq!(
        um.get(FIELD_TOTAL_TOKEN_COUNT),
        Some(&serde_json::json!(105))
    );
}

/// A system prompt must survive read→IR→write: Gemini's `systemInstruction.parts[]` reads into
/// `IrRequest.system` and writes back to the same native container (the system prompt is a headline
/// cross-protocol feature).
#[test]
fn system_instruction_round_trips_through_ir_system() {
    let body = serde_json::json!({
        "systemInstruction": {"parts": [{"text": "You are terse."}]},
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let ir = GeminiReader.read_request(&body).expect("read_request");
    assert_eq!(
        ir.system.len(),
        1,
        "systemInstruction must feed IrRequest.system"
    );
    match &ir.system[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => assert_eq!(text, "You are terse."),
        other => panic!("expected a Text system block, got {other:?}"),
    }

    let out = {
        let w = GeminiWriter;
        w.write_request(crate::codec::test_host::HOST, &ir)
    };
    assert_eq!(
        out.pointer("/systemInstruction/parts/0/text")
            .and_then(|t| t.as_str()),
        Some("You are terse."),
        "IrRequest.system must write back to systemInstruction.parts[]"
    );
}

/// `read_response` captures a part-level `thoughtSignature` sibling of `functionCall` into
/// `IrBlock::ToolUse.thought_signature`.
#[test]
fn test_read_response_captures_function_call_thought_signature() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {"name": "get_weather", "args": {"city": "SF"}},
                    "thoughtSignature": "resp-sig-abc123"
                }]
            }
        }]
    });
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(ir.content.len(), 1, "expected one ToolUse block: {ir:?}");
    match &ir.content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            thought_signature, ..
        } => assert_eq!(
            thought_signature.as_deref(),
            Some("resp-sig-abc123"),
            "the part-level thoughtSignature must be captured onto the ToolUse block"
        ),
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// `read_request` captures a part-level `thoughtSignature` from a `role:"model"` history turn
/// (a prior assistant turn replayed back on the next request, exactly Gemini's own multi-turn
/// tool-calling shape).
#[test]
fn test_read_request_captures_function_call_thought_signature_from_history() {
    let reader = GeminiReader;
    let body = serde_json::json!({
        "contents": [{
            "role": "model",
            "parts": [{
                "functionCall": {"name": "get_weather", "args": {"city": "SF"}},
                "thoughtSignature": "req-sig-xyz789"
            }]
        }]
    });
    let ir = reader.read_request(&body).expect("read_request");
    assert_eq!(ir.messages.len(), 1, "expected one message: {ir:?}");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            thought_signature, ..
        } => assert_eq!(
            thought_signature.as_deref(),
            Some("req-sig-xyz789"),
            "the part-level thoughtSignature must be captured from a model-role history turn"
        ),
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// THE OUTAGE TEST. An OpenAI-shaped `ToolUse` block (no `thought_signature` — the OpenAI wire
/// format has no such concept) is what busbar's cross-protocol seam actually produces when an
/// OpenAI client's tool-call history is replayed against a Gemini backend.
#[test]
fn test_outage_cross_protocol_tool_use_gets_sentinel_thought_signature() {
    let mut ir_req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
                // No foreign protocol has a thoughtSignature concept to read — this is exactly
                // what an OpenAI/Anthropic/etc. reader produces.
                thought_signature: None,
            }],
        }],
        ..Default::default()
    };
    super::super::chat_handle::chat_prepare_for_egress(
        &mut ir_req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: true,
            ingress_protocol: "openai",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: false,
            reasoning_budgets: [1024, 4096, 8192, 16384],
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let ir = &ir_req;
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, ir);
    assert_eq!(
        wire.pointer("/contents/0/parts/0/thoughtSignature")
            .and_then(|s| s.as_str()),
        Some("skip_thought_signature_validator"),
        "a cross-protocol functionCall with no real signature must get Google's documented \
         sentinel injected as a literal JSON string, not omitted, not base64: {wire}"
    );

    assert_eq!(
        wire.pointer("/contents/0/parts/0/functionCall/name")
            .and_then(|s| s.as_str()),
        Some("get_weather"),
    );
    assert!(
        wire.pointer("/contents/0/parts/0/functionCall/thoughtSignature")
            .is_none(),
        "thoughtSignature must be a PART-level sibling of functionCall, never nested inside it: {wire}"
    );
}

/// A real signature already present on the IR is NOT overwritten by `prepare_for_egress`'s fill
/// step — the real value always wins over the sentinel.
#[test]
fn test_prepare_for_egress_does_not_overwrite_real_thought_signature() {
    let mut ir_req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
                thought_signature: Some("a-genuinely-real-signature".to_string()),
            }],
        }],
        ..Default::default()
    };
    super::super::chat_handle::chat_prepare_for_egress(
        &mut ir_req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: true,
            ingress_protocol: "gemini",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: false,
            reasoning_budgets: [1024, 4096, 8192, 16384],
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let ir = &ir_req;
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            thought_signature, ..
        } => assert_eq!(
            thought_signature.as_deref(),
            Some("a-genuinely-real-signature"),
            "a real signature must never be overwritten by the sentinel fill"
        ),
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// VERTEX EXCLUSION: same outage scenario, but `thought_signature_fill: false` (simulating a
/// Vertex/path_base lane, which is NOT confirmed to honor the sentinel bypass).
#[test]
fn test_vertex_lane_gets_no_sentinel_thought_signature() {
    let mut ir_req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
                thought_signature: None,
            }],
        }],
        ..Default::default()
    };
    super::super::chat_handle::chat_prepare_for_egress(
        &mut ir_req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: "openai",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: false,
            reasoning_budgets: [1024, 4096, 8192, 16384],
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let ir = &ir_req;
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, ir);
    assert!(
        wire.pointer("/contents/0/parts/0/thoughtSignature")
            .is_none(),
        "a Vertex (thought_signature_fill:false) lane must emit NO thoughtSignature key at all, \
         not even an empty string: {wire}"
    );
}

/// MIXED PARALLEL CALLS: Google only signs the FIRST of N parallel function calls in a real
/// Gemini turn.
#[test]
fn test_prepare_for_egress_fills_only_missing_signatures_in_parallel_calls() {
    let mut ir_req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![
                crate::codec::ir::IrBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "SF"}),
                    cache_control: None,
                    thought_signature: Some("only-the-first-is-signed".to_string()),
                },
                crate::codec::ir::IrBlock::ToolUse {
                    id: "call_2".to_string(),
                    name: "get_time".to_string(),
                    input: serde_json::json!({"city": "SF"}),
                    cache_control: None,
                    thought_signature: None,
                },
            ],
        }],
        ..Default::default()
    };
    super::super::chat_handle::chat_prepare_for_egress(
        &mut ir_req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: true,
            ingress_protocol: "gemini",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: false,
            reasoning_budgets: [1024, 4096, 8192, 16384],
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let ir = &ir_req;
    let sigs: Vec<Option<&str>> = ir.messages[0]
        .content
        .iter()
        .map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature, ..
            } => thought_signature.as_deref(),
            _ => panic!("expected ToolUse"),
        })
        .collect();
    assert_eq!(
        sigs[0],
        Some("only-the-first-is-signed"),
        "the first call's real signature must be preserved"
    );
    assert_eq!(
        sigs[1],
        Some("skip_thought_signature_validator"),
        "the second (unsigned) parallel call must get the sentinel"
    );
    assert_ne!(sigs[0], sigs[1], "the two signatures must differ");
}

/// `write_response` emits a real signature when present, never a sentinel (response-side is never
/// touched by `prepare_for_egress`, which is request-side only), and omits the key entirely when
/// absent rather than emitting an empty string.
#[test]
fn test_write_response_emits_real_signature_never_sentinel() {
    let writer = GeminiWriter;
    let with_sig = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "SF"}),
            cache_control: None,
            thought_signature: Some("real-response-sig".to_string()),
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let wire = writer.write_response(crate::codec::test_host::HOST, &with_sig);
    assert_eq!(
        wire.pointer("/candidates/0/content/parts/0/thoughtSignature")
            .and_then(|s| s.as_str()),
        Some("real-response-sig"),
        "a real captured signature must be re-emitted verbatim: {wire}"
    );

    let without_sig = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "SF"}),
            cache_control: None,
            thought_signature: None,
        }],
        ..with_sig
    };
    let wire2 = writer.write_response(crate::codec::test_host::HOST, &without_sig);
    assert!(
        wire2
            .pointer("/candidates/0/content/parts/0/thoughtSignature")
            .is_none(),
        "write_response must never fabricate a sentinel and must omit the key when absent: {wire2}"
    );
}

/// An empty-string `thoughtSignature` on the wire is treated as absent — it normalizes to `None`
#[test]
fn test_empty_string_thought_signature_normalizes_to_none() {
    let reader = GeminiReader;
    let req_body = serde_json::json!({
        "contents": [{
            "role": "model",
            "parts": [{
                "functionCall": {"name": "get_weather", "args": {}},
                "thoughtSignature": ""
            }]
        }]
    });
    let ir = reader.read_request(&req_body).expect("read_request");
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            thought_signature, ..
        } => assert_eq!(
            *thought_signature, None,
            "an empty-string wire thoughtSignature must normalize to None"
        ),
        other => panic!("expected ToolUse, got {other:?}"),
    }

    let resp_body = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {"name": "get_weather", "args": {}},
                    "thoughtSignature": ""
                }]
            }
        }]
    });
    let resp_ir = reader
        .read_response(crate::codec::test_host::HOST, &resp_body)
        .expect("read_response");
    match &resp_ir.content[0] {
        crate::codec::ir::IrBlock::ToolUse {
            thought_signature, ..
        } => assert_eq!(
            *thought_signature, None,
            "an empty-string wire thoughtSignature must normalize to None on the response path too"
        ),
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// Regression: the truncated-usage recovery path must count Gemini thinking tokens as
/// OUTPUT, exactly as the main `gemini_usage` path does.
#[test]
fn recover_truncated_usage_counts_thinking_as_output() {
    let reader = GeminiReader;

    let tail = br#"{"usageMetadata":{"promptTokenCount":1000,"candidatesTokenCount":50,"thoughtsTokenCount":4000,"cachedContentTokenCount":200,"totalTokenCount":5050}}"#;
    let usage = reader
        .recover_truncated_usage(tail)
        .expect("usageMetadata tail must be recoverable");
    assert_eq!(
        usage.output, 4050,
        "thinking tokens must be summed into output (candidates 50 + thoughts 4000)"
    );
    assert_eq!(
        usage.input, 800,
        "input must be promptTokenCount(1000) minus cachedContentTokenCount(200)"
    );
    assert_eq!(usage.cache_read, Some(200));
}

/// Measure the nesting depth of a `Value` iteratively. A recursive walk would itself overflow the
/// stack on the very input this test exists to bound, so the measurement must not recurse.
fn value_depth(v: &serde_json::Value) -> usize {
    let mut max = 0usize;
    let mut stack = vec![(v, 1usize)];
    while let Some((node, d)) = stack.pop() {
        max = max.max(d);
        match node {
            serde_json::Value::Object(m) => {
                for child in m.values() {
                    stack.push((child, d + 1));
                }
            }
            serde_json::Value::Array(a) => {
                for child in a {
                    stack.push((child, d + 1));
                }
            }
            _ => {}
        }
    }
    max
}

/// A cycle-free "diamond" of chained `$defs` — each level references the next TWICE — has no cycle
/// for the `active` stack to catch, so an unbudgeted inliner expands it into 2^N nodes and never
/// returns.
#[test]
fn gemini_schema_ref_diamond_stays_within_budget() {
    const LEVELS: usize = 40;
    let mut defs = serde_json::Map::new();
    for i in 0..LEVELS {
        let next = format!("#/$defs/D{}", i + 1);
        defs.insert(
            format!("D{i}"),
            serde_json::json!({
                "type": "object",
                "properties": { "a": { "$ref": next }, "b": { "$ref": next } },
            }),
        );
    }
    defs.insert(
        format!("D{LEVELS}"),
        serde_json::json!({ "type": "string" }),
    );
    let schema = serde_json::json!({ "$defs": defs, "$ref": "#/$defs/D0" });

    let out = resolve_gemini_schema_refs(&schema);
    let depth = value_depth(&out);
    assert!(
        depth <= 256,
        "diamond must not expand without bound, got depth {depth}"
    );
}

/// Thousands of shallow sibling `$defs` chained singly are also cycle-free, but inline into one
/// N-deep `Value` — deep enough that the recursive walk (and the recursive drop that follows)
/// overflows the worker stack.
#[test]
fn gemini_schema_ref_flat_chain_depth_is_capped() {
    const LINKS: usize = 5_000;
    let mut defs = serde_json::Map::new();
    for i in 0..LINKS {
        defs.insert(
            format!("D{i}"),
            serde_json::json!({
                "type": "object",
                "properties": { "next": { "$ref": format!("#/$defs/D{}", i + 1) } },
            }),
        );
    }
    defs.insert(format!("D{LINKS}"), serde_json::json!({ "type": "string" }));
    let schema = serde_json::json!({ "$defs": defs, "$ref": "#/$defs/D0" });

    let out = resolve_gemini_schema_refs(&schema);
    let depth = value_depth(&out);
    assert!(
        depth <= 256,
        "chained refs must be depth-capped, got depth {depth}"
    );
}

/// The byte→char offset INDEX must answer every offset exactly as converting that offset on its own
/// does — including the two degradations the contract promises: a mid-codepoint offset resolves to
/// the nearest earlier boundary, and a past-the-end (or negative) offset clamps.
#[test]
fn gemini_char_index_matches_per_offset_conversion() {
    let text = "abc é ✓ 𝄞 déjà vu ✓✓ 𝄞𝄞 end";
    let index = GeminiCharIndex::build(text);

    let mut offsets: Vec<i64> = (-3..=(text.len() as i64 + 5)).collect();

    let n = offsets.len();
    let shuffled: Vec<i64> = (0..n).map(|i| offsets[(i * 7 + 3) % n]).collect();
    offsets = shuffled;

    for b in offsets {
        assert_eq!(
            index.char_offset(b),
            gemini_byte_offset_to_char(text, b),
            "byte offset {b} must convert identically in {text:?}"
        );
    }

    let empty = GeminiCharIndex::build("");
    for b in [-1, 0, 1, 99] {
        assert_eq!(empty.char_offset(b), gemini_byte_offset_to_char("", b));
    }
}

/// The writer's `open_tools` accumulator grows one entry per distinct tool-block index a backend
/// opens, and lives for the whole stream.
#[test]
fn test_writer_open_tools_capped_in_entry_count() {
    let writer = GeminiWriter;
    for index in 0..(MAX_GEMINI_TOOL_FRAMES + 50) {
        writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse {
                    id: String::new(),
                    name: "t".to_string(),
                },
                refusal: false,
            },
        );
    }
    let held = writer.stream.lock().open_tools.len();
    assert_eq!(
        held, MAX_GEMINI_TOOL_FRAMES,
        "open_tools must admit exactly the cap and refuse past it, got {held}"
    );
}

#[test]
fn status_word_golden() {
    let golden: [(StatusClass, u16, &str); 9] = [
        (StatusClass::RateLimit, 429, "RESOURCE_EXHAUSTED"),
        (StatusClass::Overloaded, 503, "UNAVAILABLE"),
        (StatusClass::ServerError, 500, "INTERNAL"),
        (StatusClass::Timeout, 504, "DEADLINE_EXCEEDED"),
        (StatusClass::Network, 503, "UNAVAILABLE"),
        (StatusClass::Auth, 401, "UNAUTHENTICATED"),
        (StatusClass::Billing, 403, "PERMISSION_DENIED"),
        (StatusClass::ClientError, 400, "INVALID_ARGUMENT"),
        (StatusClass::ContextLength, 400, "INVALID_ARGUMENT"),
    ];
    for (class, status, word) in golden {
        assert_eq!(
            gemini_stream_error_code_status(class),
            (status, word),
            "class={class:?}"
        );
    }
}

/// STREAMED AND BUFFERED ANSWERS NAME A CALL ALIKE, AS IN 1.5.5. For parts `[text, call, call]` the
/// second call is tool position 1: a text part before it counts once whether its block is still open
/// or already closed (v1.5.5 `crates/busbar/src/proto/gemini/reader.rs:895`,
/// `state.text_index.is_some()`).
#[test]
fn streamed_tool_ids_after_text_match_the_buffered_ids() {
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "let me check"},
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}},
                {"functionCall": {"name": "get_time", "args": {"tz": "CET"}}}
            ]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4},
        "responseId": "r-ids"
    });
    let buffered: Vec<String> = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response")
        .content
        .into_iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let streamed: Vec<String> = collect_stream(&[body])
        .into_iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::ToolUse { id, .. },
                ..
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(buffered.len(), 2, "{buffered:?}");
    assert_eq!(streamed, buffered);
}

/// A support naming one large chunk many times yields ONE citation for it, not one clone per name.
#[test]
fn grounding_repeated_chunk_index_is_cloned_once() {
    let candidate = serde_json::json!({"groundingMetadata": {
        "groundingChunks": [{"web": {"uri": "https://a", "title": "A"}}],
        "groundingSupports": [{"segment": {"startIndex": 0, "endIndex": 1, "text": "x"},
            "groundingChunkIndices": vec![0; 5000]}]
    }});
    let out = citations::read_gemini_grounding_citations(&candidate, Some("x"));
    assert_eq!(out.len(), 1);
}

/// Supports that between them name more chunk bytes than the per-candidate budget stop adding
/// citations rather than cloning the body many times over.
#[test]
fn grounding_clone_budget_bounds_total_cloned_bytes() {
    let big = "u".repeat(1024 * 1024);
    let chunks: Vec<_> = (0..16)
        .map(|_| serde_json::json!({"web": {"uri": big, "title": "T"}}))
        .collect();
    let supports: Vec<_> = (0..64)
        .map(|_| serde_json::json!({"groundingChunkIndices": (0..16).collect::<Vec<_>>()}))
        .collect();
    let candidate = serde_json::json!({"groundingMetadata": {
        "groundingChunks": chunks, "groundingSupports": supports}});
    let out = citations::read_gemini_grounding_citations(&candidate, None);
    assert!(!out.is_empty());
    assert!(
        out.len() <= 9,
        "budget must stop the clones, got {}",
        out.len()
    );
}

/// The CHARACTER to BYTE direction of the index answers as the per-offset walk does, and marks kept
/// fragment by fragment (as a stream keeps them) are the marks one pass over the whole text builds.
#[test]
fn gemini_char_index_byte_offsets_and_streamed_marks_match_the_walk() {
    let fragments: Vec<String> = (0..97)
        .map(|i| "aé✓𝄞x".repeat(i % 7) + &"z".repeat(i % 3))
        .collect();
    let mut text = String::new();
    let mut marks = Vec::new();
    let mut chars = 0usize;
    for f in &fragments {
        citations::extend_char_marks(&mut marks, &mut chars, text.len(), f);
        text.push_str(f);
    }
    assert_eq!(chars, text.chars().count());
    let built = GeminiCharIndex::build(&text);
    let kept = GeminiCharIndex::from_marks(&text, &marks);
    for b in -2..=(text.len() as i64 + 3) {
        assert_eq!(kept.char_offset(b), gemini_byte_offset_to_char(&text, b));
        assert_eq!(built.char_offset(b), kept.char_offset(b));
    }
    for c in -2..=(chars as i64 + 3) {
        assert_eq!(kept.byte_offset(c), gemini_char_offset_to_byte(&text, c));
        assert_eq!(built.byte_offset(c), kept.byte_offset(c));
    }
    assert_eq!(
        GeminiCharIndex::build("").byte_offset(5),
        gemini_char_offset_to_byte("", 5)
    );
}

/// LLM3#3 (reader): a stream whose every chunk carries answer text AND the candidate's citation
/// metadata converts each chunk's offsets against the answer kept indexed as it grew.
#[test]
fn streamed_citation_chunks_do_not_rewalk_the_answer() {
    let mut state = StreamDecodeState::default();
    let fragment = "é✓𝄞 grounded answer text ".repeat(20);
    let chunks = 300usize;
    let before = citations::CHARS_WALKED.with(std::cell::Cell::get);
    let mut cites = 0usize;
    for _ in 0..chunks {
        let chunk = serde_json::json!({"candidates": [{
            "content": {"role": "model", "parts": [{"text": fragment}]},
            "citationMetadata": {"citationSources": [
                {"startIndex": 0, "endIndex": 4, "uri": "https://example.com"}]}}]});
        for ev in
            GeminiReader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state)
        {
            if let IrStreamEvent::BlockDelta {
                delta: IrDelta::CitationsDelta(c),
                ..
            } = ev
            {
                cites += c.len();
            }
        }
    }
    let walked = citations::CHARS_WALKED.with(std::cell::Cell::get) - before;
    let answer = fragment.len() * chunks;
    assert_eq!(cites, 1, "the cumulative list is carried once");
    assert_eq!(state.streamed_chars, state.streamed_text.chars().count());
    assert!(
        walked <= 2 * answer,
        "walked {walked} characters for a {answer}-byte answer: the index was rebuilt per chunk"
    );
}

/// LLM3#3 (reader): the answer copy a stream keeps to anchor citation offsets is bounded by the
/// translate cap; text past it is still delivered, only not copied.
#[test]
fn streamed_answer_copy_is_bounded_by_the_translate_cap() {
    let cap = crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT;
    let piece = "x".repeat(4 * 1024 * 1024);
    let mut state = StreamDecodeState::default();
    let mut delivered = 0usize;
    for _ in 0..(cap / piece.len() + 2) {
        let chunk = serde_json::json!({"candidates": [{
            "content": {"role": "model", "parts": [{"text": piece}]}}]});
        for ev in
            GeminiReader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state)
        {
            if let IrStreamEvent::BlockDelta {
                delta: IrDelta::TextDelta(t),
                ..
            } = ev
            {
                delivered += t.len();
            }
        }
    }
    assert!(delivered > cap, "every text delta is still delivered");
    assert!(
        state.streamed_text.len() <= cap,
        "kept {} bytes past the {cap}-byte cap",
        state.streamed_text.len()
    );
}

/// LLM3#3 (writer): the answer copy the Gemini stream writer keeps is bounded by its translate cap.
#[test]
fn writer_stream_text_copy_is_bounded_by_the_translate_cap() {
    let writer = GeminiWriter;
    writer.set_translate_cap(1024);
    for _ in 0..10 {
        let _ = writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::TextDelta("y".repeat(200)),
            },
        );
    }
    let kept = writer.stream.lock().stream_text.text.len();
    assert!(kept <= 1024, "kept {kept} bytes past the 1024-byte cap");
}

/// LLM3#3 (writer): many streamed citation frames against one long text block convert through the
/// index the writer kept as the text grew, not a walk of the block per offset.
#[test]
fn writer_streamed_citations_do_not_rewalk_the_block() {
    let writer = GeminiWriter;
    let text = "é✓𝄞 cited answer ".repeat(2000);
    let total_chars = text.chars().count() as i64;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: IrDelta::TextDelta(text.clone()),
        },
    );
    let before = citations::CHARS_WALKED.with(std::cell::Cell::get);
    let frames = 500usize;
    for i in 0..frames {
        let at = total_chars - 1 - (i as i64 % 50);
        let citation = crate::codec::ir::IrCitation {
            url: Some("https://c".to_string()),
            start_index: Some(at),
            end_index: Some(total_chars),
            ..Default::default()
        };
        let (_, frame) = writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: IrDelta::CitationsDelta(vec![citation]),
                },
            )
            .expect("citation frame");
        let src = &frame["candidates"][0]["citationMetadata"]["citationSources"][0];
        assert_eq!(
            src["startIndex"],
            serde_json::json!(gemini_char_offset_to_byte(&text, at))
        );
        assert_eq!(src["endIndex"], serde_json::json!(text.len()));
    }
    let walked = citations::CHARS_WALKED.with(std::cell::Cell::get) - before;
    assert!(
        walked <= frames * 4 * 66,
        "walked {walked} characters for {frames} frames: the block was walked per offset"
    );
}

/// Convert a Gemini `startIndex`/`endIndex` BYTE offset (Google's `CitationSource` is documented
/// measured in bytes) into a CHARACTER offset — the IR's `IrCitation::start_index`/`end_index`
/// contract (`ir/mod.rs`).
fn gemini_byte_offset_to_char(text: &str, byte_idx: i64) -> i64 {
    let clamped = byte_idx.max(0) as usize;
    let boundary = clamped.min(text.len());

    let safe_boundary = (0..=boundary)
        .rev()
        .find(|&b| text.is_char_boundary(b))
        .unwrap_or(0);
    text[..safe_boundary].chars().count() as i64
}

/// The inverse of `gemini_byte_offset_to_char`: a CHARACTER offset (the IR contract) back to the
/// BYTE offset Gemini's wire format expects.
fn gemini_char_offset_to_byte(text: &str, char_idx: i64) -> i64 {
    let clamped = char_idx.max(0) as usize;
    match text.char_indices().nth(clamped) {
        Some((byte_idx, _)) => byte_idx as i64,
        None => text.len() as i64,
    }
}

#[test]
fn stop_key_scanner_refuses_short_and_unquoted_keys_without_panicking() {
    use crate::codec::usage_tail::{StopKey, StopKeyScanner};
    let invalid: &[&'static [u8]] = &[
        b"",
        b"x",
        b"\"",
        b"finishReason",
        b"\"finishReason",
        b"finishReason\"",
        b"\"\"",
    ];
    for &key in invalid {
        assert!(
            StopKeyScanner::new(StopKey { key, depth: 1 }).is_none(),
            "key {key:?}"
        );
    }
    let mut scanner = StopKeyScanner::new(StopKey {
        key: b"\"x\"",
        depth: 1,
    })
    .expect("a quoted one-character key is valid");
    scanner.feed(br#"{"x":"ST"#);
    assert_eq!(scanner.token(), None);
    scanner.feed(br#"OP"}"#);
    assert_eq!(scanner.token(), Some("STOP"));
}
