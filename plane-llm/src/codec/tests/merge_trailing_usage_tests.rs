// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/proto/stream.rs`.

use super::merge_trailing_usage;
use crate::codec::ir::IrUsage;

fn usage(i: u64, o: u64, cc: Option<u64>, cr: Option<u64>) -> IrUsage {
    IrUsage {
        input_tokens: i,
        output_tokens: o,
        cache_creation_input_tokens: cc,
        cache_read_input_tokens: cr,
        detail: crate::codec::ir::IrUsageDetail::default(),
    }
}

#[test]
fn trailing_nonzero_overrides_zero_leaves_intact() {
    let mut acc = usage(0, 0, None, None);
    merge_trailing_usage(&mut acc, &usage(120, 45, Some(7), Some(9)));
    assert_eq!((acc.input_tokens, acc.output_tokens), (120, 45));
    assert_eq!(acc.cache_creation_input_tokens, Some(7));
    assert_eq!(acc.cache_read_input_tokens, Some(9));

    let mut acc = usage(200, 80, Some(3), Some(5));
    merge_trailing_usage(&mut acc, &usage(0, 0, None, None));
    assert_eq!((acc.input_tokens, acc.output_tokens), (200, 80));
    assert_eq!(acc.cache_creation_input_tokens, Some(3));
    assert_eq!(acc.cache_read_input_tokens, Some(5));

    let mut acc = usage(200, 0, Some(3), None);
    merge_trailing_usage(&mut acc, &usage(0, 90, None, Some(11)));
    assert_eq!(
        acc.input_tokens, 200,
        "zero trailing input preserves the accumulator"
    );
    assert_eq!(acc.output_tokens, 90, "non-zero trailing output overrides");
    assert_eq!(
        acc.cache_creation_input_tokens,
        Some(3),
        "None trailing preserves Some"
    );
    assert_eq!(
        acc.cache_read_input_tokens,
        Some(11),
        "Some trailing overrides None"
    );
}

/// EVERY `IrUsageDetail` bucket rides the Some-wins fold, not just the handful that had a bug filed
/// against them.
#[test]
fn trailing_detail_merge_is_exhaustive_over_every_bucket() {
    let full = crate::codec::ir::IrUsageDetail {
        exact_counts: None,
        refused_counts: Vec::new(),
        exact_tool_use_prompt_tokens: None,
        reasoning_tokens: Some(1),
        cache_creation_5m_input_tokens: Some(2),
        cache_creation_1h_input_tokens: Some(3),
        cache_creation_30m_input_tokens: Some(31),
        cache_creation_12h_input_tokens: Some(32),
        search_units: Some(4),
        web_search_requests: Some(5),
        web_fetch_requests: Some(16),
        service_tier: Some("priority".to_string()),
        input_audio_tokens: Some(6),
        output_audio_tokens: Some(7),
        accepted_prediction_tokens: Some(8),
        rejected_prediction_tokens: Some(9),
        tool_use_prompt_tokens: Some(10),
        billed_input_tokens: Some(11),
        billed_output_tokens: Some(12),
        billed_classifications: Some(13),
        usage_identity_note: Some(crate::codec::ir::UsageIdentityNote {
            reported_total: 222,
            summed_total: 190,
            unaccounted: 32,
            identity: "gemini.usageMetadata",
        }),
        traffic_type: Some("ON_DEMAND".to_string()),
        create_time: Some("2026-09-07T16:54:20.179017Z".to_string()),
        by_modality: Some(crate::codec::ir::IrUsageByModality {
            input: crate::codec::ir::IrModalityCounts {
                text: Some(14),
                image: Some(15),
                ..Default::default()
            },
            ..Default::default()
        }),

        open_units: std::collections::BTreeMap::from([(
            crate::codec::ir::open_class::GUARDRAIL_TOPIC_POLICY_UNITS_CLASS.to_string(),
            14,
        )]),

        residual_units: std::collections::BTreeMap::from([("unclassed.count".to_string(), 17)]),
    };
    let mut acc = usage(0, 0, None, None);
    let mut trailing = usage(50, 20, None, None);
    trailing.detail = full.clone();
    merge_trailing_usage(&mut acc, &trailing);
    assert_eq!(
        acc.detail, full,
        "every trailing detail bucket must land on the accumulator"
    );

    let mut acc = usage(50, 20, None, None);
    acc.detail = full.clone();
    merge_trailing_usage(&mut acc, &usage(0, 0, None, None));
    assert_eq!(
        acc.detail, full,
        "an empty trailing detail must clobber no bucket"
    );
}

/// A STALE STATED-TOTAL GAP IS NEVER BILLED (LEDGER-100 makes the gap the billed open class
/// `unitemized_tokens`, REV-334 note 5).
#[test]
fn a_following_reconciled_frame_clears_an_earlier_gap() {
    let gap = crate::codec::ir::UsageIdentityNote {
        reported_total: 40,
        summed_total: 30,
        unaccounted: 10,
        identity: "gemini.usageMetadata",
    };
    let mut acc = usage(20, 10, None, None);
    acc.detail.usage_identity_note = Some(gap.clone());

    merge_trailing_usage(&mut acc, &usage(0, 0, None, None));
    assert_eq!(acc.detail.usage_identity_note, Some(gap));
    assert_eq!(
        acc.to_token_usage()
            .open_units
            .get(crate::codec::ir::open_class::UNITEMIZED_TOKENS_CLASS),
        Some(&10)
    );

    merge_trailing_usage(&mut acc, &usage(20, 20, None, None));
    assert_eq!(acc.detail.usage_identity_note, None);
    assert!(!acc
        .to_token_usage()
        .open_units
        .contains_key(crate::codec::ir::open_class::UNITEMIZED_TOKENS_CLASS));
}

#[test]
fn exact_trailing_usage_replaces_reported_zero_and_preserves_absent_input() {
    use crate::codec::usage_count::{read_usage, CountRead, CountSlot};
    const TABLE: &[(CountSlot, CountRead)] = &[
        (CountSlot::Input, CountRead::Zero(&["input"])),
        (CountSlot::Output, CountRead::Zero(&["output"])),
    ];
    let start = serde_json::from_str(r#"{"input":27.5,"output":27.1}"#).unwrap();
    let end = serde_json::from_str(r#"{"output":0}"#).unwrap();
    let mut acc = read_usage("test", Some(&start), TABLE).unwrap();
    let trailing = read_usage("test", Some(&end), TABLE).unwrap();
    merge_trailing_usage(&mut acc, &trailing);
    let exact = acc.to_token_usage().exact_counts.unwrap();
    assert_eq!(exact["input"].to_decimal_string(), "27.5");
    assert_eq!(exact["output"], busbar_contract::Count::ZERO);
}

#[test]
fn split_cache_writes_replace_reporting_aggregate_during_terminal_fold() {
    use crate::codec::ir::open_class::{CACHE_WRITE_12H_CLASS, CACHE_WRITE_30M_CLASS};
    let count = |n| busbar_contract::Count::from_integer(n).unwrap();
    let mut acc = usage(100, 0, Some(0), Some(0));
    acc.detail.exact_counts = Some(std::collections::BTreeMap::from([(
        "cache_write".into(),
        count(0),
    )]));
    let mut trailing = usage(0, 1, Some(5), Some(0));
    trailing.detail.cache_creation_30m_input_tokens = Some(2);
    trailing.detail.cache_creation_12h_input_tokens = Some(3);
    trailing.detail.exact_counts = Some(std::collections::BTreeMap::from([
        (CACHE_WRITE_30M_CLASS.into(), count(2)),
        (CACHE_WRITE_12H_CLASS.into(), count(3)),
    ]));
    merge_trailing_usage(&mut acc, &trailing);
    assert!(!acc
        .detail
        .exact_counts
        .as_ref()
        .unwrap()
        .contains_key("cache_write"));
    assert_eq!(acc.wire_count("cache_write", 0), 5);
    assert_eq!(acc.detail.cache_creation_12h_input_tokens, Some(3));
    super::backfill_from_start_usage(&mut acc, &usage(100, 0, Some(0), Some(0)));
    assert_eq!(acc.wire_count("cache_write", 0), 5);
}
