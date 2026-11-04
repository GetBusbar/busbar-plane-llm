// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `default_max_tokens` precedence and `cache_control` clamping on the IR egress-prep seam,
//! RELOCATED here from `busbar-core`'s `proxy/tests/`.

use super::*;
use crate::codec::ir::IrRequest;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

/// `default_max_tokens` resolution precedence on the translation seam (only fires when the source
/// omitted `max_tokens` AND the egress protocol REQUIRES it): per-model lane default wins → else
/// the global `limits.default_max_tokens` (the `global` arg) → else the historical 4096 (which is
/// just the value the global itself defaults to).
#[test]
fn per_model_then_global_then_4096() {
    let global = 8192;

    let prep = |proto: &'static str, lane_default: Option<u32>, global: u32| {
        crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: "openai",
            egress_requires_max_tokens: decl_for(proto).is_some_and(|d| d.requires_max_tokens),
            lane_default_max_tokens: lane_default,
            global_default_max_tokens: global,
            reasoning_allowed: true,
            reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        }
    };
    let apply = |ir: IrRequest, lane_default: Option<u32>, global: u32| -> Option<u32> {
        let mut req = ir;
        crate::codec::chat_handle::chat_prepare_for_egress(
            &mut req,
            &prep("anthropic", lane_default, global),
        );
        req.max_tokens
    };

    assert_eq!(
        apply(IrRequest::default(), Some(1234), global),
        Some(1234),
        "per-model default must win"
    );

    assert_eq!(
        apply(IrRequest::default(), None, global),
        Some(global),
        "with no per-model default, the global limit must be used"
    );

    assert_eq!(
        apply(IrRequest::default(), None, DEFAULT_MAX_TOKENS),
        Some(4096),
        "with neither per-model nor a custom global, the 4096 fallback must be used"
    );

    assert_eq!(
        apply(
            IrRequest {
                max_tokens: Some(7),
                ..IrRequest::default()
            },
            Some(1234),
            global
        ),
        Some(7),
        "an explicit caller max_tokens must be preserved over every default"
    );
}

/// Anthropic 400s with "A maximum of 4 blocks with cache_control may be provided".
#[test]
fn cache_control_breakpoints_clamped_to_four_on_anthropic_egress() {
    use crate::codec::ir::{CacheControl, CacheKind, IrBlock, IrMessage, IrRole};

    let bp = || {
        Some(CacheControl {
            kind: CacheKind::Ephemeral,
        })
    };
    let text_with = |t: &str, cc: Option<CacheControl>| IrBlock::Text {
        text: t.to_string(),
        cache_control: cc,
        citations: Vec::new(),
        refusal: false,
    };

    let ir = IrRequest {
        system: vec![text_with("sys", bp())],
        messages: vec![
            IrMessage {
                role: IrRole::User,
                content: vec![
                    text_with("m1a", bp()),
                    text_with("m1b", bp()),
                    text_with("m1c", bp()),
                ],
            },
            IrMessage {
                role: IrRole::User,
                content: vec![text_with("m2a", bp()), text_with("m2b", bp())],
            },
        ],
        ..IrRequest::default()
    };

    let count_breakpoints = |req: &IrRequest| -> usize {
        req.system
            .iter()
            .filter(|b| {
                matches!(
                    b,
                    IrBlock::Text {
                        cache_control: Some(_),
                        ..
                    }
                )
            })
            .count()
            + req
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| {
                    matches!(
                        b,
                        IrBlock::Text {
                            cache_control: Some(_),
                            ..
                        }
                    )
                })
                .count()
    };
    assert_eq!(count_breakpoints(&ir), 6, "fixture carries 6 breakpoints");

    let prep = crate::codec::egress_prep::EgressPrep {
        thought_signature_fill: false,
        ingress_protocol: "bedrock",
        egress_requires_max_tokens: true,
        lane_default_max_tokens: None,
        global_default_max_tokens: 4096,
        reasoning_allowed: true,
        reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
        prompt_caching_allowed: true,
        cache_control_cap: Some(4),
        lane_caps: Default::default(),
    };

    let seam = crate::codec::drops::Seam {
        direction: crate::codec::drops::Direction::Request,
        ingress: "bedrock",
        egress: "anthropic",
    };
    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let mut req = ir;
    tracing::dispatcher::with_default(&subscriber, || {
        crate::codec::drops::scope(seam, || {
            crate::codec::chat_handle::chat_prepare_for_egress(&mut req, &prep)
        })
    });
    let clamped = req;

    assert_eq!(
        count_breakpoints(&clamped),
        4,
        "exactly 4 breakpoints must survive the Anthropic egress cap"
    );

    assert!(
        matches!(
            &clamped.system[0],
            IrBlock::Text {
                cache_control: Some(_),
                ..
            }
        ),
        "the system breakpoint must survive (it is first in prefix order)"
    );
    assert!(
        matches!(
            &clamped.messages[1].content[0],
            IrBlock::Text {
                cache_control: None,
                ..
            }
        ),
        "m2a must be dropped (past the cap)"
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("cap of 4"))
            && diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains("dropping 2")),
        "must warn naming the cap and the dropped count: {:?}",
        diagnostic_texts(&cap)
    );
}
