// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use crate::codec::ir::{IrCitation, IrDelta, IrRole};

fn start(writer: &CohereWriter) {
    writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: IrRole::Assistant,
            id: Some("fixed".into()),
            usage: None,
            created: None,
            model: None,
        },
    );
}

fn citations(
    writer: &CohereWriter,
    index: usize,
    count: usize,
) -> Vec<(String, serde_json::Value)> {
    writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index,
            delta: IrDelta::CitationsDelta(
                (0..count)
                    .map(|n| IrCitation {
                        url: Some(format!("https://example.org/{n}")),
                        ..Default::default()
                    })
                    .collect(),
            ),
        },
    )
}

fn assert_pairs(frames: &[(String, serde_json::Value)], first: usize, count: usize) {
    assert_eq!(frames.len(), count * 2);
    for (n, pair) in frames.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0].1["type"], "citation-start");
        assert_eq!(pair[1].1["type"], "citation-end");
        assert_eq!(pair[0].1["index"], first + n);
        assert_eq!(pair[1].1["index"], first + n);
    }
}

#[test]
fn citation_ordinals_span_deltas_and_blocks_and_restart_after_message_stop() {
    let writer = CohereWriter;
    start(&writer);
    assert_pairs(&citations(&writer, 42, 2), 0, 2);
    start(&writer); // A repeated start belongs to the same message.
    assert_pairs(&citations(&writer, 42, 1), 2, 1);
    assert_pairs(&citations(&writer, 87, 2), 3, 2);
    assert!(citations(&writer, 87, 0).is_empty());
    writer.write_response_event(crate::codec::test_host::HOST, &IrStreamEvent::MessageStop);
    start(&writer);
    assert_pairs(&citations(&writer, 42, 1), 0, 1);
}

#[test]
fn exhausted_citation_ordinal_emits_error_without_orphan_end_and_resets() {
    let writer = CohereWriter;
    start(&writer);
    writer.stream.lock().next_citation_index = usize::MAX;
    let frames = citations(&writer, 0, 2);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1["type"], "message-end");
    assert_eq!(frames[0].1["delta"]["finish_reason"], "ERROR");
    start(&writer);
    assert_pairs(&citations(&writer, 0, 1), 0, 1);
}
