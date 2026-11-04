// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Wire shapes held against the PUBLISHED provider specifications pinned by
//! `testing/llm-conformance/spec-digests.tsv` (fetched into `~/.cache/busbar-llm-specs/`), not
//! against busbar's own goldens.

use super::*;
use crate::codec::ir::{IrBlock, IrResponse, IrTokenLogprob, IrUsage};

/// A minimal assistant response carrying `text`, the given stop reason and the given logprobs —
/// the smallest IR a writer will project into a full native response body.
fn resp(
    stop_reason: Option<crate::codec::ir::IrStopReason>,
    logprobs: Vec<IrTokenLogprob>,
    text: &str,
) -> IrResponse {
    IrResponse {
        logprobs,
        content: vec![IrBlock::text(text.to_string())],
        stop_reason,
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// PUBLISHED OPENAI SPEC (`openai/openai-openapi` `openapi.yaml`, the pinned commit):
#[test]
fn openai_logprobs_object_carries_the_required_refusal_member() {
    let lps = vec![IrTokenLogprob {
        token: "hi".to_string(),
        logprob: -0.25,
        bytes: None,
        bytes_null: false,
        top: Vec::new(),
    }];
    let out = crate::codec::logprob_wire::write_token_logprobs(&lps);
    assert!(
        out.get("content").is_some_and(|c| c.is_array()),
        "the content member must stay a token array: {out}"
    );
    assert_eq!(
        out.get("refusal"),
        Some(&serde_json::Value::Null),
        "logprobs requires both content and refusal; refusal is null when the model did not \
         refuse: {out}"
    );
}

/// The Responses writer LIFTS the `content` array out of the Chat logprobs object for an
/// `output_text` part's `logprobs` (Responses carries a bare `LogProb[]`, not the Chat object), so
/// adding `refusal` to the Chat object must NOT leak a stray member into the Responses part.
#[test]
fn responses_part_logprobs_stay_a_bare_array_after_the_refusal_member_lands() {
    let ir = resp(
        Some(crate::codec::ir::IrStopReason::EndTurn),
        vec![IrTokenLogprob {
            token: "hi".to_string(),
            logprob: -0.5,
            bytes: None,
            bytes_null: false,
            top: Vec::new(),
        }],
        "hi",
    );

    let writer = openai_responses::ResponsesWriter;
    let wire = writer.write_response(crate::codec::test_host::HOST, &ir);
    let part = wire
        .pointer("/output/0/content/0")
        .expect("an output_text content part");
    let lp = part
        .get("logprobs")
        .expect("supplied token logprobs must survive");
    assert!(
        lp.is_array(),
        "a Responses output_text part carries LogProb[] (an array), never the Chat \
             logprobs object: {lp}"
    );
    assert_eq!(lp.as_array().unwrap().len(), 1);
    assert_eq!(lp[0]["token"], "hi");
    assert_eq!(lp[0]["logprob"], -0.5);
}

/// PUBLISHED OPENAI SPEC: `CreateChatCompletionResponse.choices[]` declares
/// `required: ["finish_reason", "index", "message", "logprobs"]` with
/// `finish_reason: {type: string, enum: [stop, length, tool_calls, content_filter, function_call]}`
/// — NO `nullable: true`.
#[test]
fn buffered_openai_choice_finish_reason_is_never_null() {
    let ir = resp(None, Vec::new(), "oracle-marker");
    let wire = openai_chat::openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    let fr = wire
        .pointer("/choices/0/finish_reason")
        .expect("finish_reason is a required member of a buffered choice");
    assert_eq!(
        fr.as_str(),
        Some("stop"),
        "the buffered choice schema is a non-nullable enum; a backend that supplied no stop \
         reason falls back to the natural-stop token: {wire}"
    );
}

/// The STREAM chunk schema keeps `nullable: true` on `finish_reason`, so the non-final chunks must
/// still carry explicit null — the buffered fallback must not leak into the streamed path.
#[test]
fn streamed_openai_chunk_finish_reason_stays_nullable() {
    let writer = openai_chat::OpenAiWriter;
    let ev = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
            },
        )
        .expect("a text delta produces a chunk");
    assert_eq!(
        ev.1.pointer("/choices/0/finish_reason"),
        Some(&serde_json::Value::Null),
        "an intermediate chat.completion.chunk carries finish_reason null: {}",
        ev.1
    );
}

/// PUBLISHED ANTHROPIC SPEC (the Stainless-published OpenAPI document the official SDK pins in
/// `.stats.yml`): `ContentBlockStartEvent.content_block` is a `oneOf` DISCRIMINATED on `type`, whose
/// mapping is exactly `{text, thinking, redacted_thinking, tool_use, server_tool_use,
/// web_search_tool_result, web_fetch_tool_result, code_execution_tool_result,
/// bash_code_execution_tool_result, text_editor_code_execution_tool_result,
/// tool_search_tool_result, container_upload}`.
#[test]
fn anthropic_image_block_start_emits_no_frame() {
    let writer = AnthropicWriter;
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStart {
                    index: 0,
                    block: crate::codec::ir::IrBlockMeta::Image,
                    refusal: false,
                }
            )
            .is_none(),
        "image is not a member of the content_block discriminator; emit no frame"
    );

    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
            )
            .is_none(),
        "a BlockStop whose BlockStart emitted nothing must not emit an orphan content_block_stop"
    );
}

/// A TEXT block's start/stop pair must stay intact once the open-index guard exists — the guard
/// closes only what it opened, and text opens.
#[test]
fn anthropic_text_block_start_stop_pair_survives_the_guard() {
    let writer = AnthropicWriter;
    let start = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: false,
            },
        )
        .expect("a text block starts");
    assert_eq!(
        start
            .1
            .pointer("/content_block/type")
            .and_then(|v| v.as_str()),
        Some("text")
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
            )
            .is_some(),
        "the text block's content_block_stop must still be emitted"
    );
}

/// A REDACTED thinking block emits its start LATE — from the delta that carries the opaque bytes,
/// because the published `ContentBlockStartEvent` carries a redacted block's `data` inline and
/// declares no delta for it.
#[test]
fn anthropic_redacted_thinking_block_stop_follows_the_written_start() {
    let writer = AnthropicWriter;
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStart {
                    index: 0,
                    block: crate::codec::ir::IrBlockMeta::RedactedThinking,
                    refusal: false,
                }
            )
            .is_none(),
        "the redacted start is deferred to its delta"
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: crate::codec::ir::IrDelta::RedactedReasoningDelta("opaque".to_string()),
                }
            )
            .is_some(),
        "the delta writes the block's content_block_start"
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
            )
            .is_some(),
        "the written start owes its content_block_stop"
    );

    let truncated = AnthropicWriter;
    assert!(
        truncated
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStart {
                    index: 0,
                    block: crate::codec::ir::IrBlockMeta::RedactedThinking,
                    refusal: false,
                }
            )
            .is_none(),
        "the redacted start is deferred to its delta"
    );
    assert!(
        truncated
            .write_response_event(
                crate::codec::test_host::HOST,
                &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
            )
            .is_none(),
        "no content_block_start was written, so its close must not orphan one"
    );
}

/// PUBLISHED ANTHROPIC SPEC: `ErrorResponse.error` is discriminated on `type` over exactly nine
/// tokens — `invalid_request_error`, `authentication_error`, `billing_error`, `permission_error`,
/// `not_found_error`, `rate_limit_error`, `timeout_error`, `api_error`, `overloaded_error`.
#[test]
fn anthropic_error_type_stays_inside_the_published_union() {
    let writer = AnthropicWriter;
    let quota = writer.write_error(
        crate::codec::test_host::HOST,
        429,
        "insufficient_quota",
        "budget exhausted",
    );
    assert_eq!(
        quota.pointer("/error/type").and_then(|v| v.as_str()),
        Some("billing_error"),
        "billing exhaustion maps to the union's billing token: {quota}"
    );
    let ctx = writer.write_error(
        crate::codec::test_host::HOST,
        400,
        "context_length_exceeded",
        "too many tokens",
    );
    assert_eq!(
        ctx.pointer("/error/type").and_then(|v| v.as_str()),
        Some("invalid_request_error"),
        "a context overflow is a request the model cannot accept: {ctx}"
    );
    let ok = writer.write_error(
        crate::codec::test_host::HOST,
        429,
        "rate_limit",
        "slow down",
    );
    assert_eq!(
        ok.pointer("/error/type").and_then(|v| v.as_str()),
        Some("rate_limit_error"),
        "the already-correct mappings must not move: {ok}"
    );
}

/// PUBLISHED OPENAI SPEC: `FunctionToolCall` (the Responses `function_call` output item) declares
/// `required: ["type", "call_id", "name", "arguments"]`.
#[test]
fn responses_output_item_added_seeds_the_required_arguments_member() {
    let writer = openai_responses::ResponsesWriter;
    let evs = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::ToolUse {
                id: "call_abc".to_string(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
    );
    let added = evs
        .iter()
        .find(|(name, _)| name == "response.output_item.added")
        .map(|(_, v)| v)
        .expect("a tool-call BlockStart opens an output item");
    assert_eq!(
        added.pointer("/item/arguments").and_then(|v| v.as_str()),
        Some(""),
        "arguments is required on a function_call item and opens empty: {added}"
    );
    assert_eq!(
        added.pointer("/item/type").and_then(|v| v.as_str()),
        Some("function_call"),
        "the item type must not move: {added}"
    );
}
