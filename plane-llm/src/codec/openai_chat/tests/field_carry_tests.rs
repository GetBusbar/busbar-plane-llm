// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! FIELD-COVERAGE CARRY tests for the OpenAI Chat Completions dialect (`dialects/openai_chat.toml`).

use super::*;
use serde_json::json;

/// Backs: `frequency_penalty`, `presence_penalty`, `seed`, `n`, `top_p`, `logprobs`, `top_logprobs`,
#[test]
fn openai_carry_request_sampling_and_output_controls() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
        "frequency_penalty": 0.5,
        "presence_penalty": 0.3,
        "seed": 42,
        "n": 2,
        "top_p": 0.9,
        "logprobs": true,
        "top_logprobs": 5,
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "r", "schema": {"type": "object"}, "strict": true, "description": "d"
        }},
        "stop": ["END", "STOP"],
        "tool_choice": "required",
        "parallel_tool_calls": false,
        "user": "u1",
        "reasoning_effort": "high",
        "max_completion_tokens": 256
    });
    let ir = OpenAiReader.read_request(&body).expect("read");
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(out["frequency_penalty"], json!(0.5), "frequency_penalty");
    assert_eq!(out["presence_penalty"], json!(0.3), "presence_penalty");
    assert_eq!(out["seed"], json!(42), "seed");
    assert_eq!(out["n"], json!(2), "n");
    assert_eq!(out["top_p"], json!(0.9), "top_p");
    assert_eq!(out["logprobs"], json!(true), "logprobs");
    assert_eq!(out["top_logprobs"], json!(5), "top_logprobs");
    assert_eq!(
        out["response_format"]["type"],
        json!("json_schema"),
        "response_format"
    );
    assert_eq!(
        out["response_format"]["json_schema"]["name"],
        json!("r"),
        "response_format name"
    );
    assert_eq!(out["stop"], json!(["END", "STOP"]), "stop");

    assert_eq!(out["tools"][0]["type"], json!("function"), "tools type");
    assert_eq!(out["tools"][0]["function"]["name"], json!("f"), "tools");
    assert_eq!(out["tool_choice"], json!("required"), "tool_choice");
    assert_eq!(
        out["parallel_tool_calls"],
        json!(false),
        "parallel_tool_calls"
    );
    assert_eq!(out["user"], json!("u1"), "user");
    assert_eq!(out["reasoning_effort"], json!("high"), "reasoning_effort");
    assert_eq!(
        out["max_completion_tokens"],
        json!(256),
        "max_completion_tokens"
    );

    assert!(
        out.get("max_tokens").is_none(),
        "max_completion_tokens must not become max_tokens: {out}"
    );
}

/// Backs: `logit_bias`, `modalities`, `prediction`, `audio`, `service_tier`, `store`, `metadata`,
#[test]
fn openai_carry_request_provider_specific_extra_fields() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "logit_bias": {"50256": -100},
        "modalities": ["text", "audio"],
        "prediction": {"type": "content", "content": "predicted"},
        "audio": {"voice": "alloy", "format": "wav"},
        "service_tier": "scale",
        "store": true,
        "metadata": {"k": "v"},
        "stream_options": {"include_usage": true},
        "web_search_options": {"search_context_size": "high"},
        "prompt_cache_key": "pck-1",
        "safety_identifier": "sid-1",
        "verbosity": "low"
    });
    let ir = OpenAiReader.read_request(&body).expect("read");
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(out["logit_bias"], json!({"50256": -100}), "logit_bias");
    assert_eq!(out["modalities"], json!(["text", "audio"]), "modalities");
    assert_eq!(
        out["prediction"],
        json!({"type": "content", "content": "predicted"}),
        "prediction"
    );
    assert_eq!(
        out["audio"],
        json!({"voice": "alloy", "format": "wav"}),
        "audio"
    );
    assert_eq!(out["service_tier"], json!("scale"), "service_tier");
    assert_eq!(out["store"], json!(true), "store");
    assert_eq!(out["metadata"], json!({"k": "v"}), "metadata");
    assert_eq!(
        out["stream_options"],
        json!({"include_usage": true}),
        "stream_options"
    );
    assert_eq!(
        out["web_search_options"],
        json!({"search_context_size": "high"}),
        "web_search_options"
    );
    assert_eq!(out["prompt_cache_key"], json!("pck-1"), "prompt_cache_key");
    assert_eq!(
        out["safety_identifier"],
        json!("sid-1"),
        "safety_identifier"
    );
    assert_eq!(out["verbosity"], json!("low"), "verbosity");
}

/// Backs: `messages[].name`, `messages[].tool_calls`, `messages[].tool_call_id`,
#[test]
fn openai_carry_request_message_and_content_fields() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "user", "name": "alice", "content": "hi"},
            {"role": "assistant", "content": serde_json::Value::Null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
            ]},
            {"role": "assistant", "content": serde_json::Value::Null, "refusal": "I refuse"},
            {"role": "assistant", "content": serde_json::Value::Null,
             "audio": {"id": "audio_1"},
             "function_call": {"name": "g", "arguments": "{}"}},
            {"role": "tool", "tool_call_id": "call_1", "content": "result"},
            {"role": "user", "content": [
                {"type": "text", "text": "see"},
                {"type": "image_url", "image_url": {"url": "https://img/y.png"}},
                {"type": "refusal", "refusal": "no way"}
            ]}
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("read");
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    let m = out["messages"].as_array().expect("messages");

    assert_eq!(m[0]["name"], json!("alice"), "messages[].name");

    assert_eq!(
        m[1]["tool_calls"][0]["id"],
        json!("call_1"),
        "messages[].tool_calls id"
    );
    assert_eq!(
        m[1]["tool_calls"][0]["function"]["name"],
        json!("f"),
        "messages[].tool_calls name"
    );
    assert_eq!(
        m[1]["tool_calls"][0]["function"]["arguments"],
        json!("{\"a\":1}"),
        "messages[].tool_calls arguments"
    );

    assert_eq!(
        m[2]["content"][0],
        json!({"type": "refusal", "refusal": "I refuse"}),
        "messages[].refusal"
    );

    assert_eq!(m[3]["audio"]["id"], json!("audio_1"), "messages[].audio");
    assert_eq!(
        m[3]["function_call"]["name"],
        json!("g"),
        "messages[].function_call"
    );

    assert_eq!(m[4]["role"], json!("tool"), "tool message role");
    assert_eq!(
        m[4]["tool_call_id"],
        json!("call_1"),
        "messages[].tool_call_id"
    );

    assert_eq!(
        m[5]["content"][1]["image_url"]["url"],
        json!("https://img/y.png"),
        "content image_url.url"
    );

    let last_text: Vec<String> = m[5]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(String::from))
        .collect();
    assert!(
        last_text.iter().any(|t| t == "no way"),
        "content refusal.refusal must survive: {out}"
    );

    assert!(
        out.get(MESSAGE_EXTRAS_SENTINEL).is_none(),
        "extras sentinel leaked: {out}"
    );
    assert!(
        out.get(MESSAGE_NAMES_SENTINEL).is_none(),
        "names sentinel leaked: {out}"
    );
}

/// Backs: `response/id`, `response/object`, `response/created`, `response/model`,
#[test]
fn openai_carry_response_identity_and_choice_fields() {
    let body = json!({
        "id": "chatcmpl-x", "object": "chat.completion", "created": 123, "model": "gpt-4o",
        "system_fingerprint": "fp_1",
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "logprobs": {"content": [{"token": "hi", "logprob": -0.1, "bytes": [104, 105], "top_logprobs": []}]},
            "message": {
                "role": "assistant",
                "content": "hello",
                "reasoning_content": "thinking",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}],
                "annotations": [{"type": "url_citation", "url_citation": {"url": "https://a", "title": "A", "start_index": 0, "end_index": 5}}]
            }
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert!(
        ir.content.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Thinking { text, .. } if text == "thinking"
        )),
        "choices[].message.reasoning_content must reach the IR: {:?}",
        ir.content
    );

    assert!(
        ir.content.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Text { citations, .. }
                if citations.iter().any(|c| c.url.as_deref() == Some("https://a"))
        )),
        "choices[].message.annotations must reach the IR as a citation: {:?}",
        ir.content
    );

    let out = openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["id"], json!("chatcmpl-x"), "response/id");
    assert_eq!(out["object"], json!("chat.completion"), "response/object");
    assert_eq!(out["created"], json!(123), "response/created");
    assert_eq!(out["model"], json!("gpt-4o"), "response/model");
    assert_eq!(
        out["system_fingerprint"],
        json!("fp_1"),
        "response/system_fingerprint"
    );
    assert_eq!(out["choices"][0]["index"], json!(0), "choices[].index");
    assert_eq!(
        out["choices"][0]["finish_reason"],
        json!("tool_calls"),
        "choices[].finish_reason"
    );
    assert_eq!(
        out["choices"][0]["logprobs"]["content"][0]["token"],
        json!("hi"),
        "choices[].logprobs"
    );
    assert_eq!(
        out["choices"][0]["message"]["role"],
        json!("assistant"),
        "message.role"
    );
    assert_eq!(
        out["choices"][0]["message"]["content"],
        json!("hello"),
        "message.content"
    );
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["id"],
        json!("call_1"),
        "message.tool_calls"
    );
}

/// Backs: `choices[].message.refusal`. A structured-outputs / safety refusal arrives as
#[test]
fn openai_carry_response_message_refusal() {
    let body = json!({
        "id": "chatcmpl-r", "object": "chat.completion", "created": 1, "model": "gpt-4o",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": serde_json::Value::Null, "refusal": "I cannot help with that"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["choices"][0]["message"]["refusal"],
        json!("I cannot help with that"),
        "choices[].message.refusal must survive in its own slot: {out}"
    );
    assert!(
        out["choices"][0]["message"]["content"].is_null(),
        "a refused turn carries no content: {out}"
    );
}

/// Backs: `usage.prompt_tokens_details.cached_tokens`, `usage.prompt_tokens_details.audio_tokens`,
#[test]
fn openai_carry_response_usage_sub_buckets() {
    let body = json!({
        "id": "chatcmpl-u", "object": "chat.completion", "created": 1, "model": "gpt-4o",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "hi"}}],
        "usage": {
            "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 20, "audio_tokens": 10},
            "completion_tokens_details": {
                "audio_tokens": 5, "accepted_prediction_tokens": 7, "rejected_prediction_tokens": 3
            }
        }
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");

    assert_eq!(
        ir.usage.cache_read_input_tokens,
        Some(20),
        "cached_tokens → IR"
    );
    assert_eq!(
        ir.usage.detail.input_audio_tokens,
        Some(10),
        "input audio_tokens → IR"
    );
    assert_eq!(
        ir.usage.detail.output_audio_tokens,
        Some(5),
        "output audio_tokens → IR"
    );
    assert_eq!(
        ir.usage.detail.accepted_prediction_tokens,
        Some(7),
        "accepted_prediction_tokens → IR"
    );
    assert_eq!(
        ir.usage.detail.rejected_prediction_tokens,
        Some(3),
        "rejected_prediction_tokens → IR"
    );

    let out = openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["usage"]["prompt_tokens_details"]["cached_tokens"],
        json!(20),
        "usage.prompt_tokens_details.cached_tokens"
    );
    assert_eq!(
        out["usage"]["prompt_tokens_details"]["audio_tokens"],
        json!(10),
        "usage.prompt_tokens_details.audio_tokens"
    );
    assert_eq!(
        out["usage"]["completion_tokens_details"]["audio_tokens"],
        json!(5),
        "usage.completion_tokens_details.audio_tokens"
    );
    assert_eq!(
        out["usage"]["completion_tokens_details"]["accepted_prediction_tokens"],
        json!(7),
        "usage.completion_tokens_details.accepted_prediction_tokens"
    );
    assert_eq!(
        out["usage"]["completion_tokens_details"]["rejected_prediction_tokens"],
        json!(3),
        "usage.completion_tokens_details.rejected_prediction_tokens"
    );
}

/// Backs: `stream:choices[].delta.role`, `stream:choices[].delta.content`,
#[test]
fn openai_carry_stream_delta_fields() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let ev = OpenAiReader.read_response_events(crate::codec::test_host::HOST,
        "",
        &json!({"id": "chatcmpl-s", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
        &mut state,
    );
    assert!(
        ev.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                ..
            }
        )),
        "delta.role must open the stream as assistant: {ev:?}"
    );
    let (_, start_chunk) = openai_writer()
        .write_response_event(
            crate::codec::test_host::HOST,
            ev.iter()
                .find(|e| matches!(e, IrStreamEvent::MessageStart { .. }))
                .unwrap(),
        )
        .expect("MessageStart chunk");
    assert_eq!(
        start_chunk["choices"][0]["delta"]["role"],
        json!("assistant"),
        "stream:choices[].delta.role"
    );

    let ev = OpenAiReader.read_response_events(crate::codec::test_host::HOST,
        "",
        &json!({"choices": [{"index": 0, "delta": {"reasoning_content": "think"}, "finish_reason": null}]}),
        &mut state,
    );
    assert!(
        ev.iter().any(|e| matches!(
            e, IrStreamEvent::BlockDelta { delta: crate::codec::ir::IrDelta::ThinkingDelta(t), .. } if t == "think"
        )),
        "stream:choices[].delta.reasoning_content must reach a ThinkingDelta: {ev:?}"
    );

    let ev = OpenAiReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]}),
        &mut state,
    );
    let text_ev = ev
        .iter()
        .find(|e| {
            matches!(
                e,
                IrStreamEvent::BlockDelta {
                    delta: crate::codec::ir::IrDelta::TextDelta(_),
                    ..
                }
            )
        })
        .expect("a TextDelta");
    let (_, content_chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, text_ev)
        .expect("content chunk");
    assert_eq!(
        content_chunk["choices"][0]["delta"]["content"],
        json!("hi"),
        "stream:choices[].delta.content"
    );

    let mut refusal_state = crate::codec::ir::StreamDecodeState::default();
    let _ = OpenAiReader.read_response_events(crate::codec::test_host::HOST,
        "",
        &json!({"id": "chatcmpl-f", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
        &mut refusal_state,
    );
    let ev = OpenAiReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"choices": [{"index": 0, "delta": {"refusal": "no"}, "finish_reason": null}]}),
        &mut refusal_state,
    );
    assert!(
        ev.iter().any(|e| matches!(
            e, IrStreamEvent::BlockDelta { delta: crate::codec::ir::IrDelta::TextDelta(t), .. } if t == "no"
        )),
        "stream:choices[].delta.refusal must reach a TextDelta: {ev:?}"
    );
    assert!(
        refusal_state.refusal_seen,
        "a streamed refusal must latch refusal_seen"
    );

    let mut tool_state = crate::codec::ir::StreamDecodeState::default();
    let ev = OpenAiReader.read_response_events(crate::codec::test_host::HOST,
        "",
        &json!({"id": "chatcmpl-t", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_9", "type": "function", "function": {"name": "f", "arguments": "{}"}}
                ]}, "finish_reason": null}]}),
        &mut tool_state,
    );
    let start_ev = ev
        .iter()
        .find(|e| {
            matches!(
                e,
                IrStreamEvent::BlockStart {
                    block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
                    ..
                }
            )
        })
        .expect("a ToolUse BlockStart");
    let (_, tc_chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, start_ev)
        .expect("tool_calls chunk");
    assert_eq!(
        tc_chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        json!("f"),
        "stream:choices[].delta.tool_calls"
    );
    assert_eq!(
        tc_chunk["choices"][0]["delta"]["tool_calls"][0]["id"],
        json!("call_9"),
        "stream:choices[].delta.tool_calls id"
    );
}

/// Backs: `response/service_tier`, `choices[].message.audio`.
#[test]
fn openai_response_provider_specific_fields_drop_only_on_cross_proto_reserialize() {
    let body = json!({
        "id": "chatcmpl-p", "object": "chat.completion", "created": 1, "model": "gpt-4o",
        "service_tier": "scale",
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant", "content": "hi",
            "audio": {"id": "audio_1", "data": "AAA", "transcript": "hi"}
        }}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    });

    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read must tolerate provider-specific fields");
    assert_eq!(ir.usage.output_tokens, 2, "usage still tapped for billing");

    let out = openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("service_tier"),
        Some(&json!("scale")),
        "response/service_tier is carried (OAI-03): {out}"
    );
    assert!(
        out["choices"][0]["message"].get("audio").is_none(),
        "choices[].message.audio drops on cross-protocol re-serialize (no IR carrier): {out}"
    );
}
