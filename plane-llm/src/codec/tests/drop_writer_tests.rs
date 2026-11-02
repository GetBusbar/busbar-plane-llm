// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! A WRITER'S DROP IS ON THE ONE DROP PATH (design F3 "Drops"). Inside a TRANSLATE attempt, a member

use super::tests::{prep, translate as translate_body};
use crate::codec::proto_codec::protocol_for;
use crate::codec::translate::{TranslateCodec as _, TranslateReqInput};
use busbar_contract::operation::OpVerb;
use busbar_test_kit::warn_capture::WarnCapture;
use serde_json::{json, Value};

/// Translate `body` from `ingress` onto `egress`: what the seam audits, and the warns.
fn translate(ingress: &str, egress: &str, body: &Value) -> (Vec<String>, WarnCapture) {
    let _ = protocol_for(ingress);
    let handler = crate::codec::decl_of(ingress)
        .and_then(|d| d.handler)
        .and_then(|h| h.operation_handler(OpVerb::CHAT))
        .expect("a chat handler");
    let prep = prep(ingress);
    let cap = WarnCapture::default();
    let out = tracing::subscriber::with_default(cap.clone(), || {
        handler.translate_request(
            crate::codec::test_host::HOST,
            TranslateReqInput::Json(body),
            Some(egress),
            &prep,
            "m",
        )
    });
    let Ok(out) = out else {
        panic!("{ingress} -> {egress}: the request must translate");
    };
    (out.dropped_controls, cap)
}

/// Translate the far end's answer `body` (`egress`) for a caller of `ingress`: the audit rows and

fn answer(egress: &str, ingress: &str, body: &Value) -> (Vec<String>, WarnCapture) {
    let _ = protocol_for(egress);
    let ctx = crate::exchange::reply::whole::WholeCtx {
        ingress,
        egress,
        operation: OpVerb::CHAT,
        model: "m",
        wants_stream: false,
        json_array: false,
        request: None,
        now_s: 1,
        elapsed_ms: None,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let bytes = serde_json::to_vec(body).unwrap();
    let cap = WarnCapture::default();
    let whole = tracing::subscriber::with_default(cap.clone(), || {
        crate::exchange::reply::whole::translate(crate::codec::test_host::HOST, &ctx, 200, &bytes)
    });
    (whole.dropped, cap)
}

/// The one captured warn that carries exactly `path=<path>` and contains `text`.
fn warn_of(cap: &WarnCapture, path: &str, text: &str) -> Option<String> {
    let field = format!("path={path}");
    cap.messages()
        .into_iter()
        .find(|m| m.contains(text) && m.split_whitespace().any(|w| w == field))
}

/// The writer's drop is audited as `path` and warned once on the drop path with `diag`, keeping the

fn on_the_drop_path(audited: &[String], cap: &WarnCapture, path: &str, diag: &str, text: &str) {
    assert!(
        audited.iter().any(|a| a == path),
        "the writer's drop is audited as `{path}`: {audited:?}"
    );
    let warn = warn_of(cap, path, text).unwrap_or_else(|| {
        panic!(
            "one drop-path warn names `{path}` with the writer's text `{text}`: {:?}",
            cap.messages()
        )
    });
    assert!(
        warn.contains(&format!("diag={diag}")),
        "the drop-path warn carries {diag}: {warn}"
    );
    assert_eq!(
        cap.messages().iter().filter(|m| m.contains(text)).count(),
        1,
        "the writer's warn is said once, on the drop path: {:?}",
        cap.messages()
    );
}

/// A Gemini caller's reasoning-off ask (`thinkingBudget: 0`), which carries no ask across dialects

#[test]
fn a_request_member_drop_is_named_by_the_callers_row() {
    let (audited, cap) = translate(
        "gemini",
        "openai",
        &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "generationConfig.thinkingConfig.thinkingBudget",
        "BUSBAR-7079",
        "dropping thinkingBudget 0 (reasoning off) across dialects",
    );
}

/// anthropic writer: a Responses caller's `input_image` by OpenAI `file_id` has no Anthropic source,

#[test]
fn a_request_block_drop_is_named_by_the_callers_block_container() {
    let (audited, cap) = translate(
        "responses",
        "anthropic",
        &json!({"model": "m", "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "hi"},
            {"type": "input_image", "file_id": "file-1"}
        ]}]}),
    );
    let text = "dropping unresolvable vendor-scoped image reference on Anthropic egress";
    on_the_drop_path(
        &audited,
        &cap,
        "input[].content[].type=input_image",
        "BUSBAR-7085",
        text,
    );
    let warn = warn_of(&cap, "input[].content[].type=input_image", text).unwrap();
    assert!(
        warn.contains("direction=request"),
        "the exact native tagged container is named in the request warning: {warn}"
    );
}

/// openai_chat response writer: an Anthropic answer's thinking block has no Chat completion field,

#[test]
fn an_answer_drop_is_named_by_the_far_ends_block_container() {
    let (audited, cap) = answer(
        "anthropic",
        "openai",
        &json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [
                {"type": "thinking", "thinking": "hm", "signature": "s"},
                {"type": "text", "text": "hi"}
            ],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}),
    );
    let text = "dropping a reasoning/thinking block on OpenAI Chat egress";
    on_the_drop_path(
        &audited,
        &cap,
        "content[].type=thinking",
        "BUSBAR-7085",
        text,
    );
    let warn = warn_of(&cap, "content[].type=thinking", text).unwrap();
    assert!(warn.contains("direction=response"), "{warn}");
}

#[test]
fn gemini_writer_drop_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "openai",
        "gemini",
        &json!({"model": "m", "parallel_tool_calls": true,
            "messages": [{"role": "user", "content": "hi"}]}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "parallel_tool_calls",
        "BUSBAR-7085",
        "dropping parallel_tool_calls on Gemini egress",
    );
}

#[test]
fn cohere_writer_drop_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "responses",
        "cohere",
        &json!({"model": "m", "parallel_tool_calls": false, "input": "hi"}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "parallel_tool_calls",
        "BUSBAR-7085",
        "dropping parallel_tool_calls on Cohere egress",
    );
}

#[test]
fn bedrock_writer_drop_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "responses",
        "bedrock",
        &json!({"model": "m", "parallel_tool_calls": true, "input": "hi"}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "parallel_tool_calls",
        "BUSBAR-7085",
        "dropping parallel_tool_calls on Bedrock egress",
    );
}

#[test]
fn responses_writer_drop_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "anthropic",
        "responses",
        &json!({"model": "m", "max_tokens": 10, "tool_choice": {"type": "auto"},
            "messages": [{"role": "user", "content": "hi"}]}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "tool_choice",
        "BUSBAR-7085",
        "dropping tool_choice on Responses egress",
    );
}

/// Outside a TRANSLATE attempt (a direct writer call) the writer's own warn is said unchanged, with

#[test]
fn outside_an_attempt_the_writers_own_warn_is_said() {
    use crate::codec::proto_codec::ProtocolWriter as _;
    let ir = crate::codec::ir::IrRequest {
        parallel_tool_calls: Some(true),
        ..Default::default()
    };
    let cap = WarnCapture::default();
    let writer = crate::codec::gemini::GeminiWriter;
    tracing::subscriber::with_default(cap.clone(), || {
        writer.write_request(crate::codec::test_host::HOST, &ir)
    });
    let text = "dropping parallel_tool_calls on Gemini egress";
    assert_eq!(cap.count(text), 1, "{:?}", cap.messages());
    assert!(
        !cap.contains("path=") && !cap.contains("diag="),
        "no drop-path fields outside an attempt: {:?}",
        cap.messages()
    );
}

/// The caller's request is refused by its own dialect's reader: the seam answers in that

fn refused_natively(ingress: &str, egress: &str, body: &Value) {
    let _ = protocol_for(ingress);
    let handler = crate::codec::decl_of(ingress)
        .and_then(|d| d.handler)
        .and_then(|h| h.operation_handler(OpVerb::CHAT))
        .expect("a chat handler");
    let prep = prep(ingress);
    let out = handler.translate_request(
        crate::codec::test_host::HOST,
        TranslateReqInput::Json(body),
        Some(egress),
        &prep,
        "m",
    );
    assert!(
        matches!(
            out,
            Err(crate::codec::translate::TranslateReqReject::Ingress(
                busbar_contract::codec::IngressReject::BadRequest(_)
            ))
        ),
        "{ingress} -> {egress}: wrong-typed input to a mapped field is refused in the caller's \
         own error envelope, never translated"
    );
}

fn anthropic_image(media_type: Value) -> Value {
    json!({"model": "m", "max_tokens": 100, "messages": [{"role": "user", "content": [
        {"type": "text", "text": "what is this"},
        {"type": "image", "source": {"type": "base64", "media_type": media_type, "data": "QQ=="}}
    ]}]})
}

/// Bedrock writer: an image whose media type is outside Converse's `ImageFormat` union is DROPPED

#[test]
fn bedrock_drops_an_image_format_it_has_no_member_for_never_png() {
    let (wire, audited, cap) =
        translate_body("anthropic", "bedrock", &anthropic_image(json!("image/bmp")));
    on_the_drop_path(
        &audited,
        &cap,
        "messages[].content[].type=image",
        "BUSBAR-7085",
        "dropping image block on Bedrock egress",
    );
    let content = wire.pointer("/messages/0/content").expect("content");
    assert!(
        !content.to_string().contains("\"image\""),
        "no image block reaches Bedrock: {content}"
    );
    assert!(
        !wire.to_string().contains("\"png\""),
        "nothing is relabelled png: {wire}"
    );
}

/// Bedrock writer: a format that maps is carried (`image/jpg` is the union's `jpeg`), no drop.
#[test]
fn bedrock_carries_an_image_format_that_maps() {
    for (mt, want) in [
        ("image/jpg", "jpeg"),
        ("image/png", "png"),
        ("image/webp", "webp"),
    ] {
        let (wire, audited, _) =
            translate_body("anthropic", "bedrock", &anthropic_image(json!(mt)));
        assert_eq!(
            wire.pointer("/messages/0/content/1/image/format"),
            Some(&json!(want)),
            "{mt}: {wire}"
        );
        assert!(audited.is_empty(), "{mt}: nothing dropped: {audited:?}");
    }
}

/// Wrong-typed `media_type` (a mapped field) is Anthropic's own 400, not an image read as `""`

#[test]
fn a_wrong_typed_image_media_type_is_the_callers_error() {
    refused_natively("anthropic", "bedrock", &anthropic_image(json!(5)));
    refused_natively(
        "anthropic",
        "bedrock",
        &anthropic_image(json!({"type": "png"})),
    );
}

fn gemini_budget(budget: Value) -> Value {
    json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"thinkingConfig": {"thinkingBudget": budget}}})
}

/// Gemini's "the model decides" (`thinkingBudget: -1`) has no OpenAI form: DROPPED on the drop

#[test]
fn a_gemini_dynamic_budget_is_dropped_never_replaced_by_medium() {
    for (egress, member, text) in [
        (
            "openai",
            "reasoning_effort",
            "reasoning ask on OpenAI Chat egress: reasoning_effort has no dynamic form",
        ),
        (
            "responses",
            "reasoning",
            "reasoning ask on Responses egress: reasoning.effort has no dynamic form",
        ),
        (
            "anthropic",
            "thinking",
            "reasoning ask on Anthropic egress: this lane does not declare adaptive thinking",
        ),
    ] {
        let (wire, audited, cap) = translate_body("gemini", egress, &gemini_budget(json!(-1)));
        on_the_drop_path(
            &audited,
            &cap,
            "generationConfig.thinkingConfig.thinkingBudget",
            "BUSBAR-7079",
            text,
        );
        assert!(
            wire.get(member).is_none(),
            "{egress}: no `{member}` put in the dropped ask's place: {wire}"
        );
    }
}

/// A Gemini budget that maps is carried: a positive count bucketizes onto OpenAI's word, and

#[test]
fn a_gemini_budget_that_maps_is_carried() {
    let (wire, audited, _) = translate_body("gemini", "openai", &gemini_budget(json!(16384)));
    assert_eq!(wire["reasoning_effort"], "high", "{wire}");
    assert!(
        !audited.iter().any(|a| a.contains("thinkingBudget")),
        "{audited:?}"
    );

    let (wire, _, _) = translate_body("gemini", "openai", &gemini_budget(json!("16384")));
    assert_eq!(wire["reasoning_effort"], "high", "{wire}");
    let (wire, audited, _) = translate_body("gemini", "cohere", &gemini_budget(json!(-1)));
    assert_eq!(wire["thinking"], json!({"type": "enabled"}), "{wire}");
    assert!(
        !audited.iter().any(|a| a.contains("thinkingBudget")),
        "{audited:?}"
    );
}

/// Wrong-typed `thinkingBudget` (a mapped field) is Gemini's own 400, never silently left out

#[test]
fn a_wrong_typed_gemini_budget_is_the_callers_error() {
    for bad in [
        json!("lots"),
        json!(true),
        json!(1.5),
        json!(-2),
        json!({"n": 1}),
    ] {
        refused_natively("gemini", "openai", &gemini_budget(bad));
    }
}

#[test]
fn a_request_readers_unknown_detail_word_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "openai",
        "gemini",
        &json!({"model": "m", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QQ==", "detail": "ultra"}}
        ]}]}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "messages[].content[].image_url.detail",
        "BUSBAR-7085",
        "dropping an unknown image_url.detail word",
    );
}

#[test]
fn a_request_readers_unknown_effort_word_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "openai",
        "anthropic",
        &json!({"model": "m", "reasoning_effort": "turbo",
            "messages": [{"role": "user", "content": "hi"}]}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "reasoning_effort",
        "BUSBAR-7079",
        "reasoning_effort value has no IR word",
    );
}

#[test]
fn a_request_readers_unmodelled_tool_choice_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "responses",
        "anthropic",
        &json!({"model": "m", "input": "hi", "tool_choice": {"type": "web_search_preview"}}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "tool_choice",
        "BUSBAR-7085",
        "dropping Responses tool_choice on ir parse",
    );
}

#[test]
fn a_request_readers_unknown_modality_is_on_the_drop_path() {
    let (audited, cap) = translate(
        "gemini",
        "openai",
        &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"responseModalities": ["TEXT", "SMELL"]}}),
    );
    on_the_drop_path(
        &audited,
        &cap,
        "generationConfig.responseModalities[]",
        "BUSBAR-7085",
        "gemini responseModalities entry has no IR modality",
    );
}

/// An ANSWER reader's drop: audited as `path` and warned once on the drop path with `diag`. (The

fn on_the_answer_drop_path(
    audited: &[String],
    cap: &WarnCapture,
    path: &str,
    diag: &str,
    text: &str,
) {
    assert!(
        audited.iter().any(|a| a == path),
        "the reader's drop is audited as `{path}`: {audited:?}"
    );
    let on_path: Vec<String> = cap
        .messages()
        .into_iter()
        .filter(|m| m.contains(text) && m.split_whitespace().any(|w| w == format!("path={path}")))
        .collect();
    assert_eq!(on_path.len(), 1, "one drop-path warn: {:?}", cap.messages());
    assert!(
        on_path[0].contains(&format!("diag={diag}")),
        "{}",
        on_path[0]
    );
}

#[test]
fn an_answer_readers_extra_choices_are_on_the_drop_path() {
    let (audited, cap) = answer(
        "openai",
        "anthropic",
        &json!({"id": "c", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [
                {"index": 0, "message": {"role": "assistant", "content": "a"}, "finish_reason": "stop"},
                {"index": 1, "message": {"role": "assistant", "content": "b"}, "finish_reason": "stop"}
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}),
    );
    on_the_answer_drop_path(
        &audited,
        &cap,
        "choices[]",
        "BUSBAR-7085",
        "openai response carried multiple choices",
    );
}

#[test]
fn an_answer_readers_extra_candidates_are_on_the_drop_path() {
    let (audited, cap) = answer(
        "gemini",
        "openai",
        &json!({"candidates": [
                {"content": {"role": "model", "parts": [{"text": "a"}]}, "finishReason": "STOP", "index": 0},
                {"content": {"role": "model", "parts": [{"text": "b"}]}, "finishReason": "STOP", "index": 1}
            ],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}}),
    );
    on_the_answer_drop_path(
        &audited,
        &cap,
        "candidates[]",
        "BUSBAR-7085",
        "gemini response carried multiple candidates",
    );
}

#[test]
fn an_answer_readers_bedrock_only_member_is_on_the_drop_path() {
    let (audited, cap) = answer(
        "bedrock",
        "openai",
        &json!({"output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
            "stopReason": "end_turn",
            "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2},
            "trace": {"guardrail": {"modelOutput": []}}}),
    );
    on_the_answer_drop_path(
        &audited,
        &cap,
        "trace",
        "BUSBAR-7085",
        "dropping Bedrock-only Converse response member `trace`",
    );
}

#[test]
fn an_answer_readers_request_echo_is_on_the_drop_path() {
    let (audited, cap) = answer(
        "responses",
        "openai",
        &json!({"id": "resp_1", "object": "response", "created_at": 1, "model": "m",
            "status": "completed", "instructions": "be brief",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "hi", "annotations": []}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}),
    );
    on_the_answer_drop_path(
        &audited,
        &cap,
        "instructions",
        "BUSBAR-7085",
        "dropping response `instructions` echo",
    );
}

#[test]
fn equal_vendor_images_from_distinct_native_containers_keep_distinct_drop_paths() {
    let (audited, cap) = translate(
        "responses",
        "anthropic",
        &json!({
            "model": "fixture-model", "input": [
                {"type": "input_image", "file_id": "fixture-file"},
                {"type": "message", "role": "user", "content": [{"type": "input_image", "file_id": "fixture-file"}]},
                {"type": "function_call_output", "call_id": "fixture-call", "output": [{"type": "input_image", "file_id": "fixture-file"}]}
            ]
        }),
    );
    for path in [
        "input[].type=input_image",
        "input[].content[].type=input_image",
        "input[].type=function_call_output.output[].type=input_image",
    ] {
        assert!(
            audited.iter().any(|actual| actual == path),
            "native container {path}: {audited:?}"
        );
        assert!(warn_of(
            &cap,
            path,
            "dropping unresolvable vendor-scoped image reference"
        )
        .is_some());
    }
}

#[test]
fn text_only_tool_writers_report_the_actual_child_and_preserve_adjacent_text() {
    let body = json!({"model": "fixture-model", "input": [
        {"type": "function_call_output", "call_id": "fixture-call", "output": [
            {"type": "input_text", "text": "left"},
            {"type": "input_image", "file_id": "fixture-file"},
            {"type": "input_text", "text": "right"}
        ]}
    ]});
    let child_path = "input[].type=function_call_output.output[].type=input_image";
    let text_only = json!({"model": "fixture-model", "input": [
        {"type": "function_call_output", "call_id": "fixture-call", "output": [
            {"type": "input_text", "text": "left"},
            {"type": "input_text", "text": "right"}
        ]}
    ]});
    for egress in ["openai", "cohere"] {
        let (wire, audited, cap) = translate_body("responses", egress, &body);
        assert_eq!(audited, [child_path], "{egress}: {audited:?}");
        assert!(warn_of(
            &cap,
            child_path,
            "dropping non-text block inside a tool result"
        )
        .is_some());
        assert_eq!(
            wire["messages"][0]["content"], "leftright",
            "{egress}: {wire}"
        );
        assert_eq!(wire["messages"][0]["role"], "tool", "{egress}: {wire}");
        let (control, paths, warnings) = translate_body("responses", egress, &text_only);
        assert!(paths.is_empty(), "{egress}: {paths:?}");
        assert!(!warnings.contains("dropping non-text block inside a tool result"));
        assert_eq!(wire["messages"], control["messages"]);
    }
}

fn cohere_bounds_body(parameter: &str, requested: f64) -> Value {
    let mut body = json!({"model":"m", "max_tokens":16,
        "messages":[{"role":"user", "content":"hi"}]});
    body[parameter] = json!(requested);
    body
}

fn cohere_bounds_wire(parameter: &str, value: f64) -> Value {
    let mut wire = json!({"max_tokens":16, "messages":[{"role":"user", "content":"hi"}]});
    wire[if parameter == "top_p" { "p" } else { parameter }] = json!(value);
    wire
}

fn cohere_bounds_cross_dialect(parameter: &str, cases: &[(f64, f64)]) {
    for &(requested, clamped) in cases {
        let body = cohere_bounds_body(parameter, requested);
        let (wire, audited, cap) = translate_body("openai", "cohere", &body);
        assert_eq!(wire, cohere_bounds_wire(parameter, clamped), "{body}");
        assert_eq!(audited, [parameter], "{body}");
        let warnings = cap.messages();
        assert_eq!(warnings.len(), 1, "{body}: {warnings:?}");
        let warning = &warnings[0];
        for field in [
            format!("path={parameter}"),
            format!("parameter={parameter}"),
            format!("requested_value={requested}"),
            format!("clamped_value={clamped}"),
            "direction=request".into(),
            "ingress=openai".into(),
            "egress=cohere".into(),
            "diag=BUSBAR-7085".into(),
        ] {
            assert!(warning.contains(&field), "missing {field}: {warning}");
        }
        assert!(!warning.contains("temperature"), "{warning}");
    }
}

#[test]
fn cohere_bounds_cross_dialect_top_p_is_warned_and_audited() {
    cohere_bounds_cross_dialect("top_p", &[(0.0, 0.01), (1.0, 0.99)]);
}

#[test]
fn cohere_bounds_cross_dialect_frequency_penalty_is_warned_and_audited() {
    cohere_bounds_cross_dialect("frequency_penalty", &[(-0.5, 0.0), (1.5, 1.0)]);
}

#[test]
fn cohere_bounds_cross_dialect_presence_penalty_is_warned_and_audited() {
    cohere_bounds_cross_dialect(
        "presence_penalty",
        &[(-0.5, 0.0), (1.0001, 1.0), (1.5, 1.0)],
    );
}

#[test]
fn cohere_bounds_valid_endpoints_interior_and_absence_do_not_warn_or_audit() {
    for (parameter, values) in [
        ("top_p", [0.01, 0.5, 0.99]),
        ("frequency_penalty", [0.0, 0.5, 1.0]),
        ("presence_penalty", [0.0, 0.5, 1.0]),
    ] {
        for requested in values {
            let body = cohere_bounds_body(parameter, requested);
            let (wire, audited, cap) = translate_body("openai", "cohere", &body);
            assert_eq!(wire, cohere_bounds_wire(parameter, requested), "{body}");
            assert!(audited.is_empty(), "{body}: {audited:?}");
            assert!(cap.messages().is_empty(), "{body}: {:?}", cap.messages());
        }
    }
    let body = json!({"model":"m", "max_tokens":16,
        "messages":[{"role":"user", "content":"hi"}]});
    let (wire, audited, cap) = translate_body("openai", "cohere", &body);
    assert_eq!(
        wire,
        json!({"max_tokens":16, "messages":[{"role":"user", "content":"hi"}]})
    );
    assert!(audited.is_empty(), "{audited:?}");
    assert!(cap.messages().is_empty(), "{:?}", cap.messages());
}

#[test]
fn cohere_bounds_direct_writer_diagnostics_name_the_actual_parameter_and_values() {
    for (parameter, cases) in [
        ("top_p", [(0.0, 0.01), (1.0, 0.99)]),
        ("frequency_penalty", [(-0.5, 0.0), (1.5, 1.0)]),
        ("presence_penalty", [(-0.5, 0.0), (1.5, 1.0)]),
    ] {
        for (requested, clamped) in cases {
            let body = cohere_bounds_body(parameter, requested);
            let ir = protocol_for("openai")
                .unwrap()
                .reader()
                .read_request(&body)
                .unwrap();
            let cap = WarnCapture::default();
            let wire = tracing::subscriber::with_default(cap.clone(), || {
                protocol_for("cohere")
                    .unwrap()
                    .writer()
                    .write_request(crate::codec::test_host::HOST, &ir)
            });
            assert_eq!(wire, cohere_bounds_wire(parameter, clamped), "{body}");
            let warnings = cap.messages();
            assert_eq!(warnings.len(), 1, "{body}: {warnings:?}");
            let warning = &warnings[0];
            for field in [
                format!("parameter={parameter}"),
                format!("requested_value={requested}"),
                format!("clamped_value={clamped}"),
            ] {
                assert!(warning.contains(&field), "missing {field}: {warning}");
            }
            assert!(!warning.contains("temperature"), "{warning}");
        }
    }
}

#[test]
fn cohere_bounds_temperature_diagnostic_keeps_its_compatibility_fields() {
    let body = cohere_bounds_body("temperature", 1.8);
    let ir = protocol_for("openai")
        .unwrap()
        .reader()
        .read_request(&body)
        .unwrap();
    let cap = WarnCapture::default();
    let wire = tracing::subscriber::with_default(cap.clone(), || {
        protocol_for("cohere")
            .unwrap()
            .writer()
            .write_request(crate::codec::test_host::HOST, &ir)
    });
    assert_eq!(wire, cohere_bounds_wire("temperature", 1.0));
    let warnings = cap.messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for field in [
        "parameter=temperature",
        "requested_temperature=1.8",
        "clamped_temperature=1",
    ] {
        assert!(
            warnings[0].contains(field),
            "missing {field}: {}",
            warnings[0]
        );
    }
}

#[test]
fn cohere_bounds_other_temperature_rows_keep_legacy_diagnostic_fields() {
    let body = cohere_bounds_body("temperature", 1.8);
    let ir = protocol_for("openai")
        .unwrap()
        .reader()
        .read_request(&body)
        .unwrap();
    for dialect in ["anthropic", "bedrock"] {
        let cap = WarnCapture::default();
        let wire = tracing::subscriber::with_default(cap.clone(), || {
            protocol_for(dialect)
                .unwrap()
                .writer()
                .write_request(crate::codec::test_host::HOST, &ir)
        });
        let temperature = if dialect == "bedrock" {
            &wire["inferenceConfig"]["temperature"]
        } else {
            &wire["temperature"]
        };
        assert_eq!(temperature, &json!(1.0), "{dialect}: {wire}");
        let warnings = cap.messages();
        assert_eq!(warnings.len(), 1, "{dialect}: {warnings:?}");
        for field in ["requested_temperature=1.8", "clamped_temperature=1"] {
            assert!(
                warnings[0].contains(field),
                "{dialect}: missing {field}: {}",
                warnings[0]
            );
        }
        if dialect == "anthropic" {
            assert!(
                warnings[0].contains("parameter=temperature"),
                "{}",
                warnings[0]
            );
        } else {
            assert!(!warnings[0].contains("parameter="), "{}", warnings[0]);
        }
    }
}

#[test]
fn cohere_bounds_gemini_top_p_is_audited_by_the_callers_wire_path() {
    let body = json!({"contents":[{"role":"user", "parts":[{"text":"hi"}]}],
        "generationConfig":{"topP":1.0, "maxOutputTokens":16}});
    let (wire, audited, cap) = translate_body("gemini", "cohere", &body);
    assert_eq!(wire, cohere_bounds_wire("top_p", 0.99));
    assert_eq!(audited, ["generationConfig.topP"]);
    on_the_drop_path(
        &audited,
        &cap,
        "generationConfig.topP",
        "BUSBAR-7085",
        "Cohere",
    );
    let warnings = cap.messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for text in [
        "[0.01, 0.99]",
        "parameter=top_p",
        "requested_value=1",
        "clamped_value=0.99",
        "forwarded",
    ] {
        assert!(
            warnings[0].contains(text),
            "missing {text}: {}",
            warnings[0]
        );
    }
}

#[test]
fn cohere_bounds_same_dialect_request_relay_preserves_native_bytes() {
    let bytes = br#"{ "model" : "m", "messages" : [{ "role" : "user", "content" : "hi" }], "p" : 1.0, "frequency_penalty" : 1.5, "presence_penalty" : -0.5 }"#;
    let far = crate::exchange::shaping::FarShape {
        dialect: "cohere",
        wire_model: "m",
        default_max_tokens: None,
        prompt_caching: false,
        caps: Default::default(),
        path_base: None,
    };
    let cap = WarnCapture::default();
    let relayed = tracing::subscriber::with_default(cap.clone(), || {
        crate::exchange::attempt::relay_request(far, "application/json", bytes)
    });
    assert_eq!(relayed.bytes.as_ref(), bytes);
    assert!(relayed.pristine);
    assert!(cap.messages().is_empty(), "{:?}", cap.messages());
}
