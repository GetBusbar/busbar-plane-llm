// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Cohere response field mapping.

use super::super::proto_codec::protocol_for;
use serde_json::json;

fn answer() -> serde_json::Value {
    json!({
        "id": "c-1",
        "finish_reason": "COMPLETE",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}],
                    "citations": [{"start": 0, "end": 2, "text": "hi", "content_index": 0,
                                   "sources": [{"type": "tool", "id": "t1",
                                                "tool_output": {"x": 1}}]}]},
        "usage": {"billed_units": {"input_tokens": 9, "output_tokens": 2},
                  "tokens": {"input_tokens": 12, "output_tokens": 2},
                  "cached_tokens": 4}
    })
}

fn read(body: &serde_json::Value) -> crate::codec::ir::IrResponse {
    protocol_for("cohere")
        .unwrap()
        .reader()
        .read_response(crate::codec::test_host::HOST, body)
        .expect("reads")
}

fn write(resp: &crate::codec::ir::IrResponse) -> serde_json::Value {
    protocol_for("cohere")
        .unwrap()
        .writer()
        .write_response(crate::codec::test_host::HOST, resp)
}

#[test]
fn the_cache_read_count_row_bills_inside_the_input_and_is_written() {
    let resp = read(&answer());
    assert_eq!(resp.usage.cache_read_input_tokens, Some(4));
    assert_eq!(resp.usage.input_tokens, 8);
    assert_eq!(resp.usage.billable_tokens(), 14);
    let out = write(&resp);
    assert_eq!(out["usage"]["cached_tokens"], json!(4), "{out}");
}

#[test]
fn a_cohere_answer_fills_none_of_the_answer_slots() {
    let resp = read(&answer());
    assert!(resp.safety.is_empty());
    assert!(resp.audio.is_none());
    assert!(resp.usage.detail.by_modality.is_none());
    assert!(!resp
        .content
        .iter()
        .any(|b| matches!(b, crate::codec::ir::IrBlock::HostedToolRecord { .. })));
    let cites: Vec<_> = resp
        .content
        .iter()
        .flat_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => citations.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(cites.iter().all(|c| c.file.is_none()), "{cites:?}");
}

/// A foreign answer carrying every slot reaches a Cohere client as a well-formed /v2/chat answer:
#[test]
fn foreign_answer_slots_drop_on_the_cohere_wire_and_move_no_usage() {
    let plain = read(&answer());
    let mut slotted = plain.clone();
    slotted.safety = vec![crate::codec::ir::IrSafetyVerdict {
        category: "violence".into(),
        flagged: true,
        blocked: false,
    }];
    slotted.audio = Some(crate::codec::ir::IrAudioOutput {
        data: Some("AAAA".into()),
        format: Some("wav".into()),
        transcript: Some("hi".into()),
    });
    slotted.usage.detail.by_modality = Some(crate::codec::ir::IrUsageByModality {
        input: crate::codec::ir::IrModalityCounts {
            text: Some(12),
            ..Default::default()
        },
        ..Default::default()
    });
    slotted
        .content
        .push(crate::codec::ir::IrBlock::HostedToolRecord {
            kind: crate::codec::ir::IrHostedToolKind::WebSearch,
            call_id: Some("ws_1".into()),
            status: Some("completed".into()),
            results: vec![crate::codec::ir::IrSearchResult {
                url: "https://example.com".into(),
                title: Some("ex".into()),
                snippet: None,
            }],
        });

    assert_eq!(write(&slotted), write(&plain));
}
