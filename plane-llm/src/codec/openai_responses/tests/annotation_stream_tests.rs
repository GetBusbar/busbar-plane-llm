// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Streamed Responses URL citation annotations.

use super::*;

const URL_A: &str = "https://example.com/sky";
const URL_B: &str = "https://example.org/rayleigh";

fn annotation(url: &str, title: &str, start: u64, end: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "url_citation",
        "url": url,
        "title": title,
        "start_index": start,
        "end_index": end,
    })
}

fn annotations() -> Vec<serde_json::Value> {
    vec![
        annotation(URL_A, "Why the sky is blue", 0, 15),
        annotation(URL_B, "Rayleigh scattering", 16, 30),
    ]
}

const TEXT: &str = "The sky is blue. It scatters.";

/// The completed response body both paths describe.
fn completed_response() -> serde_json::Value {
    serde_json::json!({
        "id": "resp_ann",
        "object": "response",
        "created_at": 1_720_000_000_u64,
        "model": "gpt-4o",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": TEXT,
                "annotations": annotations(),
            }]
        }],
        "usage": {"input_tokens": 5, "output_tokens": 7, "total_tokens": 12}
    })
}

/// The native event sequence for `completed_response`, in wire order: the annotation frames arrive
/// after the text deltas and before `output_text.done`.
fn stream_events() -> Vec<crate::codec::ir::IrStreamEvent> {
    let item =
        serde_json::json!({"type": "message", "id": "msg_1", "role": "assistant", "content": []});
    let mut frames: Vec<(&str, serde_json::Value)> = vec![
        (
            "response.created",
            serde_json::json!({"response": {"id": "resp_ann", "created_at": 1_720_000_000_u64, "model": "gpt-4o"}}),
        ),
        (
            "response.output_item.added",
            serde_json::json!({"output_index": 0, "item": item}),
        ),
        (
            "response.content_part.added",
            serde_json::json!({"output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({"output_index": 0, "content_index": 0, "delta": "The sky is blue. "}),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({"output_index": 0, "content_index": 0, "delta": "It scatters."}),
        ),
    ];
    for (i, a) in annotations().into_iter().enumerate() {
        frames.push((
            "response.output_text.annotation.added",
            serde_json::json!({
                "type": "response.output_text.annotation.added",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0,
                "annotation_index": i,
                "annotation": a,
            }),
        ));
    }
    frames.push((
        "response.output_text.done",
        serde_json::json!({"output_index": 0, "content_index": 0, "text": TEXT}),
    ));
    frames.push(("response.content_part.done", serde_json::json!({"output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": TEXT, "annotations": annotations()}})));
    frames.push((
        "response.output_item.done",
        serde_json::json!({"output_index": 0, "item": completed_response()["output"][0].clone()}),
    ));
    frames.push((
        "response.completed",
        serde_json::json!({"response": completed_response()}),
    ));

    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    frames
        .iter()
        .flat_map(|(t, d)| {
            reader.read_response_events(crate::codec::test_host::HOST, t, d, &mut state)
        })
        .collect()
}

#[test]
fn streamed_responses_annotations_equal_buffered_citations() {
    let buffered = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &completed_response())
        .expect("buffered read");
    let buffered_citations: Vec<crate::codec::ir::IrCitation> = buffered
        .content
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        buffered_citations.len(),
        2,
        "positive control: the buffered path reads both"
    );

    let events = stream_events();
    let streamed_citations: Vec<crate::codec::ir::IrCitation> = events
        .iter()
        .filter_map(|e| match e {
            crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::CitationsDelta(cs),
            } => Some(cs.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        streamed_citations, buffered_citations,
        "streamed annotations must equal the buffered ones: {events:?}"
    );

    let pos =
        |pred: &dyn Fn(&crate::codec::ir::IrStreamEvent) -> bool| events.iter().position(pred);
    let start = pos(&|e| {
        matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockStart { index: 0, .. }
        )
    })
    .expect("start");
    let stop = pos(&|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }))
        .expect("stop");
    for (i, e) in events.iter().enumerate() {
        if matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::CitationsDelta(_),
                ..
            }
        ) {
            assert!(
                start < i && i < stop,
                "citation delta outside its block: {events:?}"
            );
        }
    }
}

/// Chat can represent URL-only annotations without an opaque Anthropic search index.
#[test]
fn streamed_responses_annotation_reaches_chat_egress() {
    let chat = super::super::openai_chat::openai_writer();
    let urls: Vec<String> = stream_events()
        .iter()
        .filter_map(|e| chat.write_response_event(crate::codec::test_host::HOST, e))
        .flat_map(|(_, body)| {
            body.pointer("/choices/0/delta/annotations")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|a| {
            a.pointer("/url_citation/url")
                .and_then(|u| u.as_str())
                .map(String::from)
        })
        .collect();
    assert_eq!(urls, vec![URL_A.to_string(), URL_B.to_string()]);
}

/// An annotation frame for an index with no open text block has nothing to attach to: no orphan
/// `CitationsDelta` is emitted.
#[test]
fn annotation_without_open_text_block_emits_nothing() {
    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let out = reader.read_response_events(crate::codec::test_host::HOST,
        "response.output_text.annotation.added",
        &serde_json::json!({"output_index": 3, "content_index": 0, "annotation_index": 0, "annotation": annotation(URL_A, "t", 0, 1)}),
        &mut state,
    );
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn native_stream_file_annotation_frames_reach_all_finalized_responses_surfaces() {
    let annotations = serde_json::json!([
        {"type":"file_citation", "file_id":"file_1", "filename":"a.pdf", "index":4},
        {"type":"container_file_citation", "container_id":"container_1", "file_id":"file_2", "filename":"b.csv", "start_index":0, "end_index":8},
        {"type":"file_path", "file_id":"file_3", "index":9}
    ]);
    let mut completed = completed_response();
    completed["output"][0]["content"][0]["annotations"] = annotations.clone();
    let buffered = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &completed)
        .unwrap();
    let crate::codec::ir::IrBlock::Text {
        citations: expected,
        ..
    } = &buffered.content[0]
    else {
        panic!("text fixture")
    };
    let mut native = vec![
        (
            "response.created",
            serde_json::json!({"response":{"id":"resp_ann", "created_at":1720000000, "model":"gpt-4o"}}),
        ),
        (
            "response.output_item.added",
            serde_json::json!({"output_index":0, "item":{"type":"message", "role":"assistant", "content":[]}}),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({"output_index":0, "delta":TEXT}),
        ),
    ];
    for (index, annotation) in annotations.as_array().unwrap().iter().enumerate() {
        native.push(("response.output_text.annotation.added", serde_json::json!({
            "output_index":0, "content_index":0, "annotation_index":index, "annotation":annotation
        })));
    }
    native.extend([
        (
            "response.content_part.done",
            serde_json::json!({"output_index":0, "part":completed["output"][0]["content"][0]}),
        ),
        (
            "response.output_item.done",
            serde_json::json!({"output_index":0, "item":completed["output"][0]}),
        ),
        (
            "response.completed",
            serde_json::json!({"response":completed}),
        ),
    ]);
    let writer = ResponsesWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let mut frames = Vec::new();
    let mut citations = Vec::new();
    for (event, body) in native {
        for ir in ResponsesReader.read_response_events(
            crate::codec::test_host::HOST,
            event,
            &body,
            &mut state,
        ) {
            if let crate::codec::ir::IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::CitationsDelta(items),
                ..
            } = &ir
            {
                citations.extend_from_slice(items);
            }
            frames.extend(
                writer
                    .try_write_response_events(crate::codec::test_host::HOST, &ir)
                    .unwrap(),
            );
        }
    }
    assert_eq!(&citations, expected);
    for (kind, pointer) in [
        ("response.content_part.done", "/part/annotations"),
        ("response.output_item.done", "/item/content/0/annotations"),
        (
            "response.completed",
            "/response/output/0/content/0/annotations",
        ),
    ] {
        let (_, body) = frames.iter().find(|(event, _)| event == kind).expect(kind);
        assert_eq!(body.pointer(pointer), Some(&annotations), "{kind}: {body}");
    }
}
