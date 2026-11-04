// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Usage tiers and router metadata removal.

use crate::codec::usage_census::tier_usage;
use busbar_contract::billing::TokenUsage;

fn unit(usage: &busbar_contract::billing::Usage, k: &str) -> Option<u64> {
    usage.usage_units.get(k).copied()
}

/// A tier the dialect reader actually reported (including an honest zero, distinct from a
/// legitimately-absent tier — see `sparse_zero_and_absent_cache_tiers_are_both_omitted`) is priced.
#[test]
fn reported_tiers_are_priced() {
    let tu = TokenUsage {
        input: 100,
        output: 40,
        cache_read: Some(30),
        cache_creation: Some(20),
        ..Default::default()
    };
    let usage = tier_usage(&tu);
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_INPUT),
        Some(100)
    );
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_OUTPUT),
        Some(40)
    );
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_READ),
        Some(30)
    );
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_WRITE),
        Some(20)
    );
}

/// A cache tier the provider genuinely never reported (`None`, per `read_count_opt`'s absent/null
/// contract) is OMITTED from the sparse map — not billed, not present as a zero entry either.
#[test]
fn absent_cache_tiers_are_omitted_not_billed() {
    let tu = TokenUsage {
        input: 100,
        output: 40,
        cache_read: None,
        cache_creation: None,
        ..Default::default()
    };
    let usage = tier_usage(&tu);
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_READ),
        None
    );
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_WRITE),
        None
    );

    let tu_zero_output = TokenUsage {
        input: 5,
        output: 0,
        ..Default::default()
    };
    let usage = tier_usage(&tu_zero_output);
    assert_eq!(unit(&usage, busbar_contract::records::UNIT_OUTPUT), None);
    assert_eq!(unit(&usage, busbar_contract::records::UNIT_INPUT), Some(5));
}

/// An explicit reported zero (`Some(0)`) and a legitimately-absent tier (`None`) price IDENTICALLY —
/// both omitted — which is correct: neither should ever be billable money, and the map's sparse
/// contract (module doc, `wire_shim.rs`) makes no distinction between "reported zero" and
/// "not reported" once the value is a readable zero.
#[test]
fn sparse_zero_and_absent_cache_tiers_are_both_omitted() {
    let reported_zero = TokenUsage {
        input: 5,
        output: 9,
        cache_read: Some(0),
        cache_creation: Some(0),
        ..Default::default()
    };
    let absent = TokenUsage {
        input: 5,
        output: 9,
        cache_read: None,
        cache_creation: None,
        ..Default::default()
    };
    assert_eq!(tier_usage(&reported_zero), tier_usage(&absent));
}

/// END TO END, through a real dialect reader this crate owns (not cohere/openai_responses): a
/// present-but-unparseable top-level `cache_read_input_tokens`/`cache_creation_input_tokens` is
/// REFUSED on its own (the path over `usage_count::billed_count`: each usage
/// field is judged on its own): it bills nothing, the readable counts beside
/// it bill as stated, and it never reaches `tier_usage()` as a priced tier.
#[test]
fn unreadable_top_level_cache_count_refuses_before_reaching_tier_usage() {
    use crate::codec::anthropic::AnthropicReader;
    use crate::codec::proto_codec::ProtocolReader;

    let body = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 9,
            "cache_read_input_tokens": "1000",
        }
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("a refused cache_read_input_tokens refuses only itself");
    let usage = tier_usage(&ir.usage.to_token_usage());
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_READ),
        None
    );
    assert_eq!(unit(&usage, busbar_contract::records::UNIT_INPUT), Some(5));

    let body = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 9,
            "cache_creation_input_tokens": "200",
        }
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("a refused cache_creation_input_tokens refuses only itself");
    let usage = tier_usage(&ir.usage.to_token_usage());
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_WRITE),
        None
    );
    assert_eq!(unit(&usage, busbar_contract::records::UNIT_OUTPUT), Some(9));

    let body = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 9,
            "cache_read_input_tokens": 1000,
        }
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("a readable cache_read_input_tokens must read, not refuse");
    let tu = ir.usage.to_token_usage();
    let usage = tier_usage(&tu);
    assert_eq!(
        unit(&usage, busbar_contract::records::UNIT_CACHE_READ),
        Some(1000)
    );
}

#[test]
fn a_residual_never_reaches_a_bill() {
    let billed = TokenUsage {
        input: 10,
        output: 5,
        cache_read: Some(2),
        ..Default::default()
    };
    let with_residual = TokenUsage {
        residual_units: std::collections::BTreeMap::from([
            ("guardrail.inputAssessment.topicPolicyUnits".to_string(), 14),
            ("bedrock.usage.stated_total_gap".to_string(), 7),
        ]),
        ..billed.clone()
    };
    assert_eq!(
        tier_usage(&with_residual),
        tier_usage(&billed),
        "tier_usage must bill the token tiers only, never a residual"
    );
    let units = crate::exchange::reply::Units::of(Some(&with_residual), Default::default());
    assert_eq!(
        units,
        crate::exchange::reply::Units::of(Some(&billed), Default::default()),
        "Units::of must report no residual as a billed class"
    );
    assert!(units.open.is_empty(), "no residual becomes an open class");
}

/// The open classes a non-token answer ledgers, through both projections: the plane's `Units`
fn non_token_open(
    billing: busbar_contract::billing::Billing,
) -> std::collections::BTreeMap<String, u64> {
    let usage = Some(billing);
    let governance = crate::exchange::reply::wire::open_units_of(&usage);
    let units = crate::exchange::reply::Units::of(
        crate::exchange::reply::wire::token_usage_of(&usage).as_ref(),
        crate::exchange::reply::wire::open_units_of(&usage),
    );
    assert_eq!(units.open, governance, "the two projections disagree");
    assert_eq!(
        (
            units.tokens_in,
            units.tokens_out,
            units.cache_read,
            units.cache_write
        ),
        (0, 0, 0, 0),
        "a non-token answer moves no token class"
    );
    governance
}

#[test]
fn a_per_image_answer_ledgers_one_images_line() {
    let open = non_token_open(busbar_contract::billing::Billing::Images {
        count: 3,
        size: None,
        quality: None,
    });
    assert_eq!(
        open,
        std::collections::BTreeMap::from([(
            crate::codec::ir::open_class::IMAGES_CLASS.to_string(),
            3
        )])
    );
    assert!(non_token_open(busbar_contract::billing::Billing::Images {
        count: 0,
        size: None,
        quality: None,
    })
    .is_empty());
}

#[test]
fn a_transcription_duration_ledgers_one_audio_ms_line() {
    let ms = |text: &str| {
        non_token_open(busbar_contract::billing::Billing::Duration {
            seconds: busbar_contract::Count::parse(text).expect("a decimal"),
        })
        .get(crate::codec::ir::open_class::AUDIO_MS_CLASS)
        .copied()
    };
    assert_eq!(ms("9"), Some(9_000));
    assert_eq!(ms("12.3456"), Some(12_345));
    assert_eq!(ms("0.0004"), None);
    let open = non_token_open(busbar_contract::billing::Billing::Duration {
        seconds: busbar_contract::Count::parse("1.5").expect("a decimal"),
    });
    assert_eq!(open.len(), 1, "one line: {open:?}");
}

/// Neither a flat answer nor the TTS request-seam size is a reported count: no line.
#[test]
fn a_flat_answer_ledgers_no_open_class() {
    assert!(non_token_open(busbar_contract::billing::Billing::Flat).is_empty());
}

const SHIM: &str = crate::codec::gemini::GEMINI_JSON_ARRAY_SHIM_KEY;

#[test]
fn strip_router_shim_keys_removes_the_never_native_keys_per_egress() {
    use super::strip_router_shim_keys;
    use serde_json::json;
    let mut v = json!({"model": "p", "stream": true, (SHIM): true, "messages": []});
    assert!(strip_router_shim_keys(&mut v, "bedrock"));
    assert_eq!(
        v["model"], "p",
        "model NOT stripped here (rewrite_model owns it)"
    );
    assert!(v.get("stream").is_none(), "bedrock: stream shim stripped");
    assert!(
        v.get(SHIM).is_none(),
        "gemini array shim key stripped on every protocol"
    );
    assert!(v.get("messages").is_some(), "real fields retained");

    let mut v = json!({"stream": true, (SHIM): true});
    assert!(strip_router_shim_keys(&mut v, "gemini"));
    assert!(v.get("stream").is_none() && v.get(SHIM).is_none());

    let mut v = json!({"model": "gpt-4o", "stream": true, (SHIM): true});
    assert!(strip_router_shim_keys(&mut v, "openai"));
    assert_eq!(
        v["model"], "gpt-4o",
        "openai model is genuine, not stripped"
    );
    assert_eq!(v["stream"], true, "openai stream is genuine, not stripped");
    assert!(
        v.get(SHIM).is_none(),
        "gemini array key stripped even for body-model ingress"
    );
}

#[test]
fn shim_strip_then_rewrite_model_keeps_the_model_cross_protocol() {
    use super::strip_router_shim_keys;
    use serde_json::json;
    let mut v = json!({"model": "router-placeholder", "stream": true, (SHIM): true});
    strip_router_shim_keys(&mut v, "openai");
    crate::codec::proto_codec::protocol_for("openai")
        .expect("openai")
        .writer()
        .rewrite_model_if_needed(&mut v, "gpt-4o");
    assert_eq!(
        v["model"], "gpt-4o",
        "cross-protocol egress body MUST carry the authoritative model"
    );
    assert_eq!(
        v["stream"], true,
        "writer-authored `stream` survives for a body-model egress"
    );
    assert!(
        v.get(SHIM).is_none(),
        "gemini array key stripped cross-protocol"
    );

    let caller = br#"{ "contents": [ ], "z" : 1.0 }"#;
    let carried = br#"{"model":"router-placeholder","stream":true, "contents": [ ], "z" : 1.0 }"#;
    let far = crate::exchange::shaping::FarShape {
        dialect: "gemini",
        wire_model: "gemini-1.5-pro",
        default_max_tokens: None,
        prompt_caching: false,
        caps: Default::default(),
        path_base: None,
    };
    let relayed = crate::exchange::attempt::relay_request(far, "application/json", carried);
    assert_eq!(
        relayed.bytes.as_ref(),
        caller.as_slice(),
        "same-protocol gemini relay sends the caller's bytes, no body model or stream"
    );
}
