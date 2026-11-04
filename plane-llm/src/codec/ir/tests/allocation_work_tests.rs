// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use crate::codec::ir::{IrMessage, IrSystemFold, IrSystemRole};

#[test]
fn snapshot_preserves_projected_facts_and_survives_request_drop() {
    let mut request = IrRequest {
        stream: true,
        user: Some("end-user".into()),
        max_tokens: Some(123),
        system: vec![IrBlock::text("system\ntext")],
        system_turns_folded: 1,
        system_folds: vec![IrSystemFold {
            before: 0,
            role: IrSystemRole::Developer,
            blocks: 0..1,
        }],
        messages: vec![IrMessage {
            role: IrRole::Assistant,
            content: vec![
                IrBlock::ToolUse {
                    id: "id".into(),
                    name: "tool".into(),
                    input: serde_json::json!({"nested": ["é", 1]}),
                    cache_control: None,
                    thought_signature: None,
                },
                IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".repeat(100_000),
                    },
                    cache_control: None,
                    detail: None,
                },
                IrBlock::Thinking {
                    text: "opaque".repeat(100_000),
                    signature: None,
                    redacted: true,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                },
            ],
        }],
        ..Default::default()
    };
    let snapshot = request.facts_snapshot();
    assert_eq!(snapshot.content(), request.content());
    assert_eq!(snapshot.shape(), request.shape());
    assert_eq!(snapshot.wants_stream(), request.wants_stream());
    assert_eq!(snapshot.end_user(), request.end_user());
    assert_eq!(snapshot.verb(), request.verb());
    let expected_shape = request.shape();
    request.messages.clear();
    drop(request);
    assert_eq!(snapshot.shape(), expected_shape);
    assert_eq!(snapshot.end_user(), Some("end-user"));
}

#[test]
fn fold_coverage_matches_union_for_unordered_overlapping_and_invalid_ranges() {
    let ranges = [4..9, 2..6, 7..3, 100..200, 1..1, 9..10];
    let request = IrRequest {
        system: (0..12).map(|i| IrBlock::text(i.to_string())).collect(),
        system_folds: ranges
            .iter()
            .cloned()
            .map(|blocks| IrSystemFold {
                before: 0,
                role: IrSystemRole::System,
                blocks,
            })
            .collect(),
        ..Default::default()
    };
    let items = project(&request);
    let actual: Vec<_> = items
        .iter()
        .filter(|i| i.slot() == Slot::System)
        .map(|i| i.screenable_text().into_owned())
        .collect();
    let expected: Vec<_> = (0..12)
        .filter(|b| !ranges.iter().any(|r| r.contains(b)))
        .map(|b| b.to_string())
        .collect();
    assert_eq!(actual, expected);
    // Range coverage sorting must not change the original fold/turn sequence.
    let turns: Vec<_> = items
        .iter()
        .filter(|i| i.slot().turn_index().is_some())
        .map(|i| (i.slot(), i.screenable_text().into_owned()))
        .collect();
    assert_eq!(turns.first(), Some(&(Slot::Turn(0), "4".into())));
    assert!(turns.contains(&(Slot::Turn(2), String::new())));
}

#[test]
fn many_system_folds_preserve_every_turn() {
    let request = IrRequest {
        system: (0..4096).map(|i| IrBlock::text(i.to_string())).collect(),
        system_folds: (0..4096)
            .map(|i| IrSystemFold {
                before: 0,
                role: IrSystemRole::System,
                blocks: i..i + 1,
            })
            .collect(),
        ..Default::default()
    };
    let items = project(&request);
    assert_eq!(items.len(), 4096);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item.slot(), Slot::Turn(i));
        assert_eq!(item.screenable_text(), i.to_string());
    }
}
