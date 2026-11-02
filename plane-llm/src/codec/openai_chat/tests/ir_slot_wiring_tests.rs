// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR-slot WIRING tests for the OpenAI Chat dialect: the Chat reader FILLS the typed

use crate::codec::proto_codec::{ProtocolReader, ProtocolWriter};
use serde_json::{json, Value};

/// Chat body → Chat reader → the cross-protocol egress seam → Chat writer. The seam clears `extra`,
fn through_seam(body: &Value) -> Value {
    let mut req = super::OpenAiReader.read_request(body).expect("body reads");
    crate::codec::chat_handle::chat_prepare_for_egress(
        &mut req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: "responses",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: true,
            reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    super::openai_writer().write_request(crate::codec::test_host::HOST, &req)
}

fn chat_body(extra: Value) -> Value {
    let mut body = json!({
        "model": "gpt-5",
        "messages": [{"role": "user", "content": "hi"}]
    });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    body
}

fn text(t: &str, refusal: bool) -> crate::codec::ir::IrBlock {
    crate::codec::ir::IrBlock::Text {
        text: t.to_string(),
        cache_control: None,
        citations: Vec::new(),
        refusal,
    }
}

/// OAI-12 (IR-02). The Chat reader flags `message.refusal` as the refusal block, and the writer
#[test]
fn oai12_refusal_block_is_carried_exactly() {
    let body = json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "refusal": "no"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    });
    let ir = super::OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("reads");
    assert!(
        ir.content.iter().any(
            |b| matches!(b, crate::codec::ir::IrBlock::Text { refusal: true, text, .. } if text == "no")
        ),
        "{:?}",
        ir.content
    );

    let resp = crate::codec::ir::IrResponse {
        content: vec![
            text("Here is part. ", false),
            text("I can't do the rest.", true),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        ..Default::default()
    };
    let out = super::openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let msg = &out["choices"][0]["message"];
    assert_eq!(msg["content"], json!("Here is part. "), "{out}");
    assert_eq!(msg["refusal"], json!("I can't do the rest."), "{out}");
}

/// OAI-12 (IR-02), stream. `delta.refusal` opens a text block flagged `refusal: true`, and the Chat
#[test]
fn oai12_stream_refusal_is_carried_exactly() {
    let reader = super::OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let mut events = Vec::new();
    for c in [
        json!({"id": "c", "created": 1, "model": "m",
               "choices": [{"index": 0, "delta": {"role": "assistant", "refusal": "I can't"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"refusal": " help."}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ] {
        events.extend(reader.read_response_events(crate::codec::test_host::HOST, "", &c, &mut st));
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: true,
                ..
            }
        )),
        "{events:?}"
    );

    let w = super::openai_writer();
    let frames: Vec<Value> = events
        .iter()
        .filter_map(|e| {
            w.write_response_event(crate::codec::test_host::HOST, e)
                .map(|(_, v)| v)
        })
        .collect();
    let refusal: String = frames
        .iter()
        .filter_map(|f| {
            f.pointer("/choices/0/delta/refusal")
                .and_then(|v| v.as_str())
        })
        .collect();
    assert_eq!(refusal, "I can't help.", "{frames:?}");
    assert!(
        frames
            .iter()
            .all(|f| f.pointer("/choices/0/delta/content").is_none()),
        "{frames:?}"
    );
}

/// OAI-03 (IR-04). The request `service_tier` crosses in the typed slot — every OpenAI tier,
#[test]
fn oai03_flex_and_scale_tiers_cross() {
    for tier in ["auto", "default", "flex", "scale", "priority"] {
        let out = through_seam(&chat_body(json!({"service_tier": tier})));
        assert_eq!(out["service_tier"], json!(tier), "{out}");
    }
    let ir = super::OpenAiReader
        .read_request(&chat_body(json!({"service_tier": "flex"})))
        .expect("reads");
    assert_eq!(ir.service_tier, Some(crate::codec::ir::IrServiceTier::Flex));

    for tier in ["flex", "scale"] {
        let body = json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o",
            "service_tier": tier,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        });
        let ir = super::OpenAiReader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("reads");
        assert_eq!(ir.usage.detail.service_tier.as_deref(), Some(tier));
        let out = super::openai_writer().write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["service_tier"], json!(tier), "{out}");
    }
}

/// SHR-03. A Responses `input_file.file_id` (vendor tag `responses`) reaches a Chat lane as
#[test]
fn shr03_responses_file_id_reaches_chat() {
    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Media {
                kind: crate::codec::ir::IrMediaKind::Document,
                source: crate::codec::ir::IrImageSource::Vendor {
                    vendor: "responses",
                    value: json!({"file_id": "file-abc"}),
                },
                name: None,
                cache_control: None,
                citations: None,
                context: None,
            }],
        }],
        ..Default::default()
    };
    let out = super::openai_writer().write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out.pointer("/messages/0/content/0"),
        Some(&json!({"type": "file", "file": {"file_id": "file-abc"}})),
        "{out}"
    );
}

/// IR-03 / IR-05 / IR-06 / IR-07. `metadata`, `store`, `safety_identifier`, `prompt_cache_key` and
#[test]
fn ir03_05_06_07_request_scalars_cross() {
    let members = json!({
        "metadata": {"team": "search", "run": "7"},
        "store": true,
        "safety_identifier": "user-hash-1",
        "prompt_cache_key": "pck-1",
        "verbosity": "low"
    });
    let out = through_seam(&chat_body(members.clone()));
    for (k, v) in members.as_object().expect("object") {
        assert_eq!(out.get(k), Some(v), "{k}: {out}");
    }
}

/// IR-19. `modalities` crosses; `audio` is written only beside the caller's own `audio` member (a
#[test]
fn ir19_modalities_cross() {
    let members: Value = serde_json::from_str(include_str!("fixtures/chat_audio_member.json"))
        .expect("the audio member fixture is JSON");
    let ir = super::OpenAiReader
        .read_request(&chat_body(members))
        .expect("reads");
    assert_eq!(
        ir.output_modalities,
        Some(vec![
            crate::codec::ir::IrModality::Text,
            crate::codec::ir::IrModality::Audio
        ])
    );
    let same = super::openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(same["modalities"], json!(["text", "audio"]), "{same}");

    let out = through_seam(&chat_body(json!({"modalities": ["text", "audio"]})));
    assert_eq!(out["modalities"], json!(["text"]), "{out}");
}

/// IR-11. `web_search_options` is read into the neutral hosted web search and written back.
#[test]
fn ir11_web_search_options_cross() {
    let wso = json!({
        "search_context_size": "high",
        "user_location": {"type": "approximate",
                          "approximate": {"city": "Paris", "country": "FR", "timezone": "Europe/Paris"}}
    });
    let ir = super::OpenAiReader
        .read_request(&chat_body(json!({"web_search_options": wso.clone()})))
        .expect("reads");
    assert!(matches!(
        ir.hosted_tools.as_slice(),
        [crate::codec::ir::IrHostedTool::WebSearch(ws)]
            if ws.search_context_size == Some(crate::codec::ir::IrVerbosity::High)
    ));
    let out = through_seam(&chat_body(json!({"web_search_options": wso.clone()})));
    assert_eq!(out["web_search_options"], wso, "{out}");

    let req = crate::codec::ir::IrRequest {
        hosted_tools: vec![crate::codec::ir::IrHostedTool::CodeExecution],
        ..Default::default()
    };
    let w = super::openai_writer();
    assert_eq!(w.dropped_egress_controls(&req), vec!["code_execution"]);
    assert!(w
        .write_request(crate::codec::test_host::HOST, &req)
        .get("web_search_options")
        .is_none());
}

/// IR-10. `tool_choice:{type:"allowed_tools"}` crosses as the allowed subset plus its mode.
#[test]
fn ir10_allowed_tools_cross() {
    let choice = json!({"type": "allowed_tools", "allowed_tools": {"mode": "required", "tools": [
        {"type": "function", "function": {"name": "a"}}]}});
    let body = chat_body(json!({
        "tools": [
            {"type": "function", "function": {"name": "a", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "b", "parameters": {"type": "object"}}}
        ],
        "tool_choice": choice.clone()
    }));
    let ir = super::OpenAiReader.read_request(&body).expect("reads");
    assert_eq!(ir.allowed_tools, Some(vec!["a".to_string()]));
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );
    let out = through_seam(&body);
    assert_eq!(out["tool_choice"], choice, "{out}");
}

/// IR-14. A `developer` system turn is written back as `developer`; mixed roles fall back to
#[test]
fn ir14_developer_role_crosses() {
    let body = json!({"model": "gpt-5", "messages": [
        {"role": "developer", "content": "be brief"},
        {"role": "user", "content": "hi"}]});
    let out = through_seam(&body);
    assert_eq!(
        out.pointer("/messages/0/role"),
        Some(&json!("developer")),
        "{out}"
    );

    let mixed = json!({"model": "gpt-5", "messages": [
        {"role": "developer", "content": "a"}, {"role": "system", "content": "b"},
        {"role": "user", "content": "hi"}]});
    let ir = super::OpenAiReader.read_request(&mixed).expect("reads");
    assert_eq!(ir.system_role, None);
}

/// IR-08. `image_url.detail` crosses in the Image block's typed slot.
#[test]
fn ir08_image_detail_crosses() {
    let body = json!({"model": "gpt-5", "messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "https://x.test/a.png", "detail": "low"}}]}]});
    let out = through_seam(&body);
    assert_eq!(
        out.pointer("/messages/0/content/0/image_url/detail"),
        Some(&json!("low")),
        "{out}"
    );
}

/// Same-dialect fidelity. A member the typed slot cannot hold exactly (a non-string metadata value,
#[test]
fn unrepresentable_slot_members_relay_verbatim_on_the_same_dialect() {
    let body = chat_body(json!({"metadata": {"n": 1}, "service_tier": "turbo"}));
    let ir = super::OpenAiReader.read_request(&body).expect("reads");
    assert_eq!(ir.metadata, None);
    assert_eq!(ir.service_tier, None);
    let same = super::openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(same["metadata"], json!({"n": 1}), "{same}");
    assert_eq!(same["service_tier"], json!("turbo"), "{same}");

    let out = through_seam(&body);
    assert!(out.get("metadata").is_none(), "{out}");
    assert!(out.get("service_tier").is_none(), "{out}");
}

/// OAI-10 (IR-09). `reasoning_effort: "none"` is reasoning switched OFF — not "no ask" — so a
#[test]
fn oai10_none_is_reasoning_off() {
    let body = chat_body(json!({"reasoning_effort": "none"}));
    let ir = super::OpenAiReader.read_request(&body).expect("reads");
    assert_eq!(ir.reasoning, Some(crate::codec::ir::IrReasoningAsk::Off));
    let same = super::openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(same["reasoning_effort"], json!("none"), "{same}");

    let out = through_seam(&body);
    assert!(out.get("reasoning_effort").is_none(), "{out}");

    let ir = super::OpenAiReader
        .read_request(&chat_body(json!({"reasoning_effort": "xhigh"})))
        .expect("reads");
    assert_eq!(
        ir.reasoning,
        Some(crate::codec::ir::IrReasoningAsk::Effort(
            crate::codec::ir::IrReasoningEffort::XHigh
        ))
    );
}
