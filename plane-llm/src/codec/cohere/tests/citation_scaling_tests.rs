use super::*;
use crate::codec::ir::{IrBlock, IrCitation};

fn citation(start: i64, end: i64) -> IrCitation {
    IrCitation {
        start_index: Some(start),
        end_index: Some(end),
        cited_text: Some("original".into()),
        ..Default::default()
    }
}

#[test]
fn indexed_candidates_scale_with_overlaps_not_answer_size() {
    for size in [1000, 2000, 4000, 10000] {
        let blocks: Vec<_> = (0..size).map(|_| IrBlock::text("é")).collect();
        let ranges = cohere_citation_ranges(&blocks);
        assert_eq!(
            ranges.iter().map(|r| r.byte_offsets.len()).sum::<usize>(),
            size * 2
        );
        let candidates: usize = (0..size)
            .map(|n| {
                let selected = cohere_citation_overlap(&ranges, n as i64, n as i64 + 1);
                assert_eq!(selected[0].block, n);
                selected.len()
            })
            .sum();
        assert_eq!(candidates, size);
    }
    let blocks = vec![IrBlock::text("é".repeat(524288))];
    let ranges = cohere_citation_ranges(&blocks);
    assert_eq!(ranges[0].byte_offsets.len(), 524289);
    for _ in 0..10000 {
        let selected = cohere_citation_overlap(&ranges, 500000, 500001);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].byte_offsets[500000], 1000000);
    }
}

#[test]
fn indexed_attachment_preserves_overlap_clipping_zero_spans_and_fallback() {
    let texts = ["", "é好", "", "xyz", ""];
    for (start, stop) in [
        (-1, 1),
        (0, 0),
        (1, 1),
        (2, 2),
        (1, 4),
        (0, 5),
        (2, 3),
        (8, 9),
        (3, 1),
    ] {
        let original = citation(start, stop);
        let mut blocks: Vec<_> = texts.iter().map(|t| IrBlock::text(*t)).collect();
        attach_cohere_citations(
            &mut blocks,
            vec![original.clone()],
            &mut CohereCitationBudget::default(),
        )
        .unwrap();
        let mut expected: Vec<Vec<IrCitation>> = vec![vec![]; texts.len()];
        let mut base = 0;
        let mut attached = false;
        for (index, text) in texts.iter().enumerate() {
            let end = base + text.chars().count() as i64;
            if start >= 0 && stop >= start && start < end && stop > base {
                let mut local = original.clone();
                local.start_index = Some(start.max(base) - base);
                local.end_index = Some(stop.min(end) - base);
                if start < base || stop > end {
                    local.cited_text = Some(
                        text.chars()
                            .skip((start.max(base) - base) as usize)
                            .take((stop.min(end) - start.max(base)) as usize)
                            .collect(),
                    );
                }
                expected[index].push(local);
                attached = true;
            }
            base = end;
        }
        if !attached {
            expected[0].push(original);
        }
        for (block, expected) in blocks.iter().zip(expected) {
            let IrBlock::Text { citations, .. } = block else {
                unreachable!()
            };
            assert_eq!(citations, &expected, "span {start}..{stop}");
        }
    }
}

#[test]
fn missing_span_and_no_text_keep_citation_fallback() {
    let citation = IrCitation {
        cited_text: Some("unanchored".into()),
        ..Default::default()
    };
    let mut blocks = vec![];
    assert!(attach_cohere_citations(
        &mut blocks,
        vec![citation.clone()],
        &mut CohereCitationBudget::default()
    )
    .unwrap());
    assert!(!attach_cohere_citations(
        &mut blocks,
        vec![citation.clone()],
        &mut CohereCitationBudget::default()
    )
    .unwrap());
    let IrBlock::Text { citations, .. } = &blocks[0] else {
        unreachable!()
    };
    assert_eq!(citations, &vec![citation.clone(), citation]);
}

#[test]
fn oversized_metadata_expansion_fails_before_any_overlap_clone() {
    let mut blocks: Vec<_> = (0..10000).map(|_| IrBlock::text("x")).collect();
    let mut large = citation(0, 10000);
    large.raw =
        Some(serde_json::json!({"sources":[{"document":{"text":"x".repeat(1024 * 1024)}}]}));
    assert!(attach_cohere_citations(
        &mut blocks,
        vec![large],
        &mut CohereCitationBudget::default()
    )
    .is_err());
    assert!(blocks
        .iter()
        .all(|b| matches!(b, IrBlock::Text { citations, .. } if citations.is_empty())));
}

#[test]
fn readers_reject_expansion_and_response_usage_is_recoverable() {
    let parts: Vec<_> = (0..100)
        .map(|_| serde_json::json!({"type":"text","text":"x"}))
        .collect();
    let message = serde_json::json!({"role":"assistant", "content":parts, "citations":[{"start":0,"end":100,"sources":[{"document":{"text":"x".repeat(1024 * 1024)}}]}]});
    assert!(CohereReader
        .read_request(&serde_json::json!({"messages":[message.clone()]}))
        .is_err());
    let response = serde_json::json!({"message":message,"usage":{"tokens":{"input_tokens":7,"output_tokens":3}}});
    assert!(CohereReader
        .read_response(crate::codec::test_host::HOST, &response)
        .is_err());
    assert_eq!(
        CohereReader
            .recover_response_usage(&response)
            .unwrap()
            .output,
        3
    );
}

#[test]
fn citation_budget_is_cumulative_and_item_bounded() {
    let mut budget = CohereCitationBudget::default();
    budget
        .charge(crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT, 1)
        .unwrap();
    assert!(budget.charge(1, 1).is_err());
    let mut budget = CohereCitationBudget::default();
    budget.charge(0, 65536).unwrap();
    assert!(budget.charge(0, 1).is_err());
    assert!(CohereCitationBudget::default()
        .charge(usize::MAX, usize::MAX)
        .is_err());
}
