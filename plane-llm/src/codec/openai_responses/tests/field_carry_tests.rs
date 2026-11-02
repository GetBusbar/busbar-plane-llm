// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! FIELD-COVERAGE CARRY INSTRUMENTS for the `responses` (OpenAI Responses API) dialect.

use super::*;
use crate::codec_diagnostics::{diagnostic_texts, warnings, DiagnosticEvents};

fn read_req(body: &serde_json::Value) -> crate::codec::ir::IrRequest {
    ResponsesReader
        .read_request(body)
        .expect("read_request should succeed")
}

fn write_req(ir: &crate::codec::ir::IrRequest) -> serde_json::Value {
    let w = ResponsesWriter;
    w.write_request(crate::codec::test_host::HOST, ir)
}

/// Read a request JSON, translate to IR, and write it back on the SAME dialect — the round-trip a
fn roundtrip_req(body: &serde_json::Value) -> serde_json::Value {
    write_req(&read_req(body))
}

/// Every content-part object across the written `input` array (flattening message `content[]` and
fn input_parts(body: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for item in body
        .get("input")
        .and_then(|i| i.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
            for part in content {
                out.push(part.clone());
            }
        } else {
            out.push(item.clone());
        }
    }
    out
}

/// The first input part whose `type` equals `ty`.
fn part_of_type(body: &serde_json::Value, ty: &str) -> Option<serde_json::Value> {
    input_parts(body)
        .into_iter()
        .find(|p| p.get("type").and_then(|t| t.as_str()) == Some(ty))
}

fn usage(input: u64, output: u64) -> crate::codec::ir::IrUsage {
    crate::codec::ir::IrUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    }
}

fn mk_response(
    content: Vec<crate::codec::ir::IrBlock>,
    stop: Option<crate::codec::ir::IrStopReason>,
) -> crate::codec::ir::IrResponse {
    crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content,
        stop_reason: stop,
        usage: usage(3, 5),
        model: Some("gpt-carry".to_string()),
        id: Some("resp_carry".to_string()),
        created: Some(1_700_000_000),
        system_fingerprint: None,
        stop_sequence: None,
        request_echo: None,
        stop_detail: None,
        ..Default::default()
    }
}

fn write_response(resp: &crate::codec::ir::IrResponse) -> serde_json::Value {
    let w = ResponsesWriter;
    w.write_response(crate::codec::test_host::HOST, resp)
}

fn text_block(text: &str) -> crate::codec::ir::IrBlock {
    crate::codec::ir::IrBlock::text(text.to_string())
}

/// Run `f` with a warn-capturing subscriber and return whatever it produced plus the capture.
fn with_warns<T>(f: impl FnOnce() -> T) -> (T, DiagnosticEvents) {
    let (cap, cap_subscriber) = warnings();
    let sub = cap_subscriber.clone();
    let out = tracing::dispatcher::with_default(&sub, f);
    (out, cap)
}

/// Emit `ev` through `w` and return the ordered wire event-type names.
fn event_names(w: &ResponsesWriter, ev: crate::codec::ir::IrStreamEvent) -> Vec<String> {
    w.write_response_events(crate::codec::test_host::HOST, &ev)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// responses/request/{model,input,instructions,max_output_tokens,temperature,top_p,stream}
#[test]
fn responses_request_core_fields_survive_roundtrip() {
    let body = serde_json::json!({
        "model": "gpt-4.1-mini",
        "instructions": "You are terse.",
        "input": [{ "type": "input_text", "text": "ping" }],
        "max_output_tokens": 321,
        "temperature": 0.42,
        "top_p": 0.77,
        "stream": true
    });
    let out = roundtrip_req(&body);

    assert_eq!(out["model"], "gpt-4.1-mini", "model must survive: {out}");
    assert_eq!(
        out["instructions"], "You are terse.",
        "instructions must survive: {out}"
    );

    assert!(
        input_parts(&out)
            .iter()
            .any(|p| p.get("text").and_then(|t| t.as_str()) == Some("ping")),
        "input turn must survive: {out}"
    );
    assert_eq!(
        out["max_output_tokens"], 321,
        "max_output_tokens must survive: {out}"
    );
    assert_eq!(out["temperature"], 0.42, "temperature must survive: {out}");
    assert_eq!(out["top_p"], 0.77, "top_p must survive: {out}");
    assert_eq!(out["stream"], true, "stream must survive: {out}");
}

/// responses/request/{tools,tool_choice,parallel_tool_calls}
#[test]
fn responses_request_tools_survive_roundtrip() {
    let body = serde_json::json!({
        "input": [{ "type": "input_text", "text": "go" }],
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "look it up",
            "parameters": { "type": "object", "properties": { "q": { "type": "string" } } },
            "strict": true
        }],
        "tool_choice": { "type": "function", "name": "lookup" },
        "parallel_tool_calls": false
    });
    let out = roundtrip_req(&body);

    let tool = out["tools"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .expect("a tool must survive");
    assert_eq!(tool["type"], "function", "tool type: {out}");
    assert_eq!(tool["name"], "lookup", "tool name must survive: {out}");
    assert!(
        tool["parameters"]["properties"]["q"].is_object(),
        "tool parameters must survive: {out}"
    );
    assert_eq!(tool["strict"], true, "tool strict must survive: {out}");

    assert_eq!(out["tool_choice"]["type"], "function", "tool_choice: {out}");
    assert_eq!(
        out["tool_choice"]["name"], "lookup",
        "tool_choice target name must survive: {out}"
    );

    assert_eq!(
        out["parallel_tool_calls"], false,
        "parallel_tool_calls must survive: {out}"
    );
}

/// responses/request/{text.format,text.verbosity}
#[test]
fn responses_request_text_format_and_verbosity_survive_roundtrip() {
    let body = serde_json::json!({
        "input": [{ "type": "input_text", "text": "structured" }],
        "text": {
            "format": {
                "type": "json_schema",
                "name": "Weather",
                "schema": { "type": "object", "properties": { "t": { "type": "number" } } },
                "strict": true
            },
            "verbosity": "high"
        }
    });
    let out = roundtrip_req(&body);

    assert_eq!(
        out["text"]["format"]["type"], "json_schema",
        "text.format.type must survive: {out}"
    );
    assert_eq!(
        out["text"]["format"]["name"], "Weather",
        "text.format.name must survive: {out}"
    );
    assert!(
        out["text"]["format"]["schema"]["properties"]["t"].is_object(),
        "text.format.schema must survive: {out}"
    );
    assert_eq!(
        out["text"]["format"]["strict"], true,
        "text.format.strict must survive: {out}"
    );

    assert_eq!(
        out["text"]["verbosity"], "high",
        "text.verbosity must survive alongside format: {out}"
    );
}

/// responses/request/{reasoning.effort,reasoning.summary}
#[test]
fn responses_request_reasoning_survives_roundtrip() {
    let body = serde_json::json!({
        "input": [{ "type": "input_text", "text": "think" }],
        "reasoning": { "effort": "high", "summary": "auto" }
    });
    let out = roundtrip_req(&body);

    assert_eq!(
        out["reasoning"]["effort"], "high",
        "reasoning.effort must survive: {out}"
    );
    assert_eq!(
        out["reasoning"]["summary"], "auto",
        "reasoning.summary must survive: {out}"
    );
}

/// responses/request/{max_tool_calls,top_logprobs,store,stream_options,truncation,include,metadata,
#[test]
fn responses_request_provider_specific_fields_survive_same_protocol() {
    let body = serde_json::json!({
        "input": [{ "type": "input_text", "text": "x" }],
        "max_tool_calls": 4,
        "top_logprobs": 3,
        "store": false,
        "stream_options": { "include_usage": true },
        "truncation": "auto",
        "include": ["reasoning.encrypted_content"],
        "metadata": { "trace": "abc123" },
        "prompt": { "id": "pmpt_1", "version": "2" },
        "prompt_cache_key": "cache-key-9",
        "safety_identifier": "user-hash-7",
        "service_tier": "priority",
        "background": true,
        "user": "end-user-5"
    });
    let out = roundtrip_req(&body);

    assert_eq!(out["max_tool_calls"], 4, "max_tool_calls: {out}");
    assert_eq!(out["top_logprobs"], 3, "top_logprobs: {out}");
    assert_eq!(out["store"], false, "store: {out}");
    assert_eq!(
        out["stream_options"]["include_usage"], true,
        "stream_options: {out}"
    );
    assert_eq!(out["truncation"], "auto", "truncation: {out}");
    assert_eq!(
        out["include"][0], "reasoning.encrypted_content",
        "include: {out}"
    );
    assert_eq!(out["metadata"]["trace"], "abc123", "metadata: {out}");
    assert_eq!(out["prompt"]["id"], "pmpt_1", "prompt: {out}");
    assert_eq!(
        out["prompt_cache_key"], "cache-key-9",
        "prompt_cache_key: {out}"
    );
    assert_eq!(
        out["safety_identifier"], "user-hash-7",
        "safety_identifier: {out}"
    );
    assert_eq!(out["service_tier"], "priority", "service_tier: {out}");
    assert_eq!(out["background"], true, "background: {out}");
    assert_eq!(out["user"], "end-user-5", "user: {out}");
}

/// responses/request/content[].type={input_text.text,output_text.text,input_image.image_url,
#[test]
fn responses_request_content_parts_survive_roundtrip() {
    let body = serde_json::json!({
        "input": [
            { "type": "input_text", "text": "user says hi" },
            { "type": "input_image", "image_url": "data:image/png;base64,QUJD" },
            { "type": "input_image", "file_id": "file-42" },
            { "type": "output_text", "text": "assistant said hi" }
        ]
    });
    let out = roundtrip_req(&body);
    let parts = input_parts(&out);

    assert!(
        parts.iter().any(
            |p| p.get("type").and_then(|t| t.as_str()) == Some("input_text")
                && p.get("text").and_then(|t| t.as_str()) == Some("user says hi")
        ),
        "input_text.text must survive: {out}"
    );
    assert!(
        parts.iter().any(
            |p| p.get("type").and_then(|t| t.as_str()) == Some("output_text")
                && p.get("text").and_then(|t| t.as_str()) == Some("assistant said hi")
        ),
        "output_text.text must survive: {out}"
    );
    assert!(
        parts.iter().any(
            |p| p.get("type").and_then(|t| t.as_str()) == Some("input_image")
                && p.get("image_url").and_then(|u| u.as_str())
                    == Some("data:image/png;base64,QUJD")
        ),
        "input_image.image_url must survive: {out}"
    );
    assert!(
        parts.iter().any(
            |p| p.get("type").and_then(|t| t.as_str()) == Some("input_image")
                && p.get("file_id").and_then(|f| f.as_str()) == Some("file-42")
        ),
        "input_image.file_id must survive: {out}"
    );
}

/// responses/request/content[].type=input_image.detail — IR-08: the detail rides the IR
#[test]
fn responses_request_input_image_detail_carried() {
    let body = serde_json::json!({
        "input": [{ "type": "input_image", "image_url": "data:image/png;base64,QUJD", "detail": "high" }]
    });
    let ir = read_req(&body);
    assert!(
        ir.messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(
                b,
                crate::codec::ir::IrBlock::Image {
                    detail: Some(crate::codec::ir::IrImageDetail::High),
                    ..
                }
            )),
        "the image block must carry detail high: {:?}",
        ir.messages
    );

    let unknown = serde_json::json!({
        "input": [{ "type": "input_image", "image_url": "data:image/png;base64,QUJD", "detail": "original" }]
    });
    let (ir, cap) = with_warns(|| read_req(&unknown));
    assert!(
        ir.messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Image { detail: None, .. })),
        "the image survives an unknown detail word: {:?}",
        ir.messages
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("detail")),
        "dropping an unknown input_image.detail must warn, naming the field: {:?}",
        diagnostic_texts(&cap)
    );
}

/// responses/request/content[].type=output_text.annotations — a prior-turn assistant `output_text`
#[test]
fn responses_request_output_text_annotations_survive() {
    let body = serde_json::json!({
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "grounded claim",
                "annotations": [{
                    "type": "url_citation",
                    "url": "https://src.example/doc",
                    "title": "Doc",
                    "start_index": 0,
                    "end_index": 14
                }]
            }]
        }]
    });
    let ir = read_req(&body);

    let citation = ir
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } if !citations.is_empty() => {
                Some(&citations[0])
            }
            _ => None,
        })
        .expect("assistant history citation");
    assert_eq!(
        citation.url.as_deref(),
        Some("https://src.example/doc"),
        "output_text.annotations url must be carried into IR citations: {:?}",
        ir.messages
    );
    assert_eq!(citation.title.as_deref(), Some("Doc"));
    assert_eq!(
        (citation.start_index, citation.end_index),
        (Some(0), Some(14))
    );
}

/// responses/request/input[].type={function_call.*,function_call_output.*}
#[test]
fn responses_request_function_call_items_survive_roundtrip() {
    let body = serde_json::json!({
        "input": [
            {
                "type": "function_call",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\":\"Paris\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_abc",
                "output": "sunny, 21C"
            }
        ]
    });
    let out = roundtrip_req(&body);

    let fc = part_of_type(&out, "function_call").expect("function_call item must survive");
    assert_eq!(fc["call_id"], "call_abc", "function_call.call_id: {out}");
    assert_eq!(fc["name"], "get_weather", "function_call.name: {out}");

    assert_eq!(
        fc["arguments"].as_str().map(|s| s.contains("Paris")),
        Some(true),
        "function_call.arguments must survive: {out}"
    );

    let fco =
        part_of_type(&out, "function_call_output").expect("function_call_output must survive");
    assert_eq!(
        fco["call_id"], "call_abc",
        "function_call_output.call_id: {out}"
    );
    assert_eq!(
        fco["output"], "sunny, 21C",
        "function_call_output.output must survive: {out}"
    );
}

/// responses/request/input[].type=reasoning.{summary,content,encrypted_content}
#[test]
fn responses_request_reasoning_items_survive_roundtrip() {
    let body = serde_json::json!({
        "input": [{
            "type": "reasoning",
            "id": "rs_carrytest_1",
            "summary": [{ "type": "summary_text", "text": "SUMMARY_MARK" }],
            "content": [{ "type": "reasoning_text", "text": "CONTENT_MARK" }],
            "encrypted_content": "ENC_BLOB_MARK"
        }]
    });

    let (out, _cap) = with_warns(|| roundtrip_req(&body));

    let rs = out["input"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|it| it.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        })
        .cloned()
        .expect("reasoning item must survive");

    let content_text = rs["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        content_text.contains("CONTENT_MARK"),
        "reasoning.content text must survive: {out}"
    );

    assert!(
        content_text.contains("SUMMARY_MARK"),
        "reasoning.summary text must survive the hop: {out}"
    );
    assert_eq!(
        rs["encrypted_content"], "ENC_BLOB_MARK",
        "reasoning.encrypted_content must survive: {out}"
    );
}

/// responses/request/input[].type=reasoning.id — the opaque reasoning-item id has no IR slot and no
#[test]
fn responses_request_reasoning_id_dropped_with_warn() {
    let body = serde_json::json!({
        "input": [{
            "type": "reasoning",
            "id": "rs_carrytest_1",
            "content": [{ "type": "reasoning_text", "text": "deep" }],
            "encrypted_content": "BLOB"
        }]
    });
    let (ir, cap) = with_warns(|| read_req(&body));

    assert!(
        ir.messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Thinking { .. })),
        "the reasoning text/blob must survive even as the id drops: {:?}",
        ir.messages
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("rs_carrytest_1")),
        "dropping reasoning-item id must warn, naming the id: {:?}",
        diagnostic_texts(&cap)
    );
}

/// responses/response/{id,object,created_at,model,status}
#[test]
fn responses_response_identity_fields_emitted() {
    let out = write_response(&mk_response(
        vec![text_block("hi")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    ));

    assert_eq!(out["id"], "resp_carry", "response.id must survive: {out}");
    assert_eq!(out["object"], "response", "response.object: {out}");
    assert_eq!(
        out["created_at"], 1_700_000_000u64,
        "response.created_at must survive: {out}"
    );
    assert_eq!(
        out["model"], "gpt-carry",
        "response.model must survive: {out}"
    );
    assert_eq!(out["status"], "completed", "response.status: {out}");
}

/// E8 (tracker): the pinned spec's `Response` schema REQUIRES `temperature`/`top_p`/`instructions`/
#[test]
fn responses_response_request_echo_members_mirror_the_client_request_buffered() {
    let mut resp = mk_response(
        vec![text_block("hi")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    );
    resp.request_echo = Some(serde_json::json!({
        "model": "gpt-carry",
        "input": "hi",
        "temperature": 0.31,
        "top_p": 0.42,
        "instructions": "be terse",
        "metadata": {"session": "abc123"},
        "tool_choice": "required",
        "parallel_tool_calls": false,
        "tools": [{"type": "function", "name": "lookup", "parameters": {}}],
    }));
    let out = write_response(&resp);

    assert_eq!(
        out["temperature"], 0.31,
        "temperature must echo the client's value: {out}"
    );
    assert_eq!(
        out["top_p"], 0.42,
        "top_p must echo the client's value: {out}"
    );
    assert_eq!(
        out["instructions"], "be terse",
        "instructions must echo the client's value: {out}"
    );
    assert_eq!(
        out["metadata"],
        serde_json::json!({"session": "abc123"}),
        "metadata must echo the client's value: {out}"
    );
    assert_eq!(
        out["tool_choice"], "required",
        "tool_choice must echo the client's value: {out}"
    );
    assert_eq!(
        out["parallel_tool_calls"], false,
        "parallel_tool_calls must echo the client's value: {out}"
    );
    assert_eq!(
        out["tools"],
        serde_json::json!([{"type": "function", "name": "lookup", "parameters": {}}]),
        "tools must echo the client's value: {out}"
    );
}

/// E8: absent `request_echo` (no parsed ingress body was available to thread) falls back to the
#[test]
fn responses_response_request_echo_absent_falls_back_to_spec_defaults() {
    let out = write_response(&mk_response(
        vec![text_block("hi")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    ));
    assert_eq!(out["temperature"], 1.0, "default temperature: {out}");
    assert_eq!(out["top_p"], 1.0, "default top_p: {out}");
    assert_eq!(
        out["instructions"],
        serde_json::Value::Null,
        "default instructions: {out}"
    );
    assert_eq!(
        out["metadata"],
        serde_json::json!({}),
        "default metadata: {out}"
    );
    assert_eq!(out["tool_choice"], "auto", "default tool_choice: {out}");
    assert_eq!(
        out["parallel_tool_calls"], true,
        "default parallel_tool_calls: {out}"
    );
    assert_eq!(out["tools"], serde_json::json!([]), "default tools: {out}");
}

/// E8: a `null` echoed member (the client's wire body carried the key as JSON `null`, distinct from
#[test]
fn responses_response_request_echo_null_member_falls_back_to_default() {
    let mut resp = mk_response(
        vec![text_block("hi")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    );
    resp.request_echo = Some(serde_json::json!({"temperature": null, "top_p": null}));
    let out = write_response(&resp);
    assert_eq!(
        out["temperature"], 1.0,
        "null echo falls back to default: {out}"
    );
    assert_eq!(out["top_p"], 1.0, "null echo falls back to default: {out}");
}

/// E8 (tracker): the STREAMING twin — `set_request_echo` (called once by the cross-protocol stream
#[test]
fn responses_response_request_echo_members_mirror_the_client_request_streaming() {
    let w = ResponsesWriter;
    w.set_request_echo(&serde_json::json!({
        "temperature": 0.55,
        "top_p": 0.9,
        "instructions": "streaming instructions",
        "metadata": {"k": "v"},
        "tool_choice": "none",
        "parallel_tool_calls": false,
    }));
    let frames = w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_stream".to_string()),
            created: Some(1_700_000_000),
            model: Some("gpt-carry".to_string()),
        },
    );
    let (_, data) = frames.first().expect("response.created must emit a frame");
    let response = &data["response"];
    assert_eq!(
        response["temperature"], 0.55,
        "streaming response.created must echo temperature: {data}"
    );
    assert_eq!(
        response["top_p"], 0.9,
        "streaming response.created must echo top_p: {data}"
    );
    assert_eq!(
        response["instructions"], "streaming instructions",
        "streaming response.created must echo instructions: {data}"
    );
    assert_eq!(
        response["metadata"],
        serde_json::json!({"k": "v"}),
        "streaming response.created must echo metadata: {data}"
    );
    assert_eq!(
        response["tool_choice"], "none",
        "streaming response.created must echo tool_choice: {data}"
    );
    assert_eq!(
        response["parallel_tool_calls"], false,
        "streaming response.created must echo parallel_tool_calls: {data}"
    );
}

/// responses/response/{output,output_text}
#[test]
fn responses_response_output_and_output_text_emitted() {
    let out = write_response(&mk_response(
        vec![text_block("Hello "), text_block("world")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    ));

    let output = out["output"].as_array().expect("output must be an array");
    assert!(
        output
            .iter()
            .any(|it| it.get("type").and_then(|t| t.as_str()) == Some("message")),
        "output must carry message item(s): {out}"
    );

    let reconstructed: String = output
        .iter()
        .filter(|it| it.get("type").and_then(|t| t.as_str()) == Some("message"))
        .flat_map(|it| it["content"].as_array().cloned().unwrap_or_default())
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()).map(String::from))
        .collect();
    assert_eq!(
        reconstructed, "Hello world",
        "output_text (SDK aggregate of output[]) data must survive: {out}"
    );

    assert!(
        out.get("output_text").is_none(),
        "output_text must NOT be a raw wire key (SDK-computed only): {out}"
    );
}

/// responses/response/error
#[test]
fn responses_response_error_field_carried() {
    let ok = write_response(&mk_response(
        vec![text_block("hi")],
        Some(crate::codec::ir::IrStopReason::EndTurn),
    ));
    assert!(
        ok.get("error").is_some() && ok["error"].is_null(),
        "a successful response must carry error:null: {ok}"
    );

    let failed = serde_json::json!({
        "status": "failed",
        "output": [],
        "error": { "code": "rate_limit_exceeded", "message": "slow down" }
    });
    let err = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &failed)
        .expect_err("a failed body must surface as an error");
    assert_eq!(
        err.provider_signal.as_deref(),
        Some("rate_limit_exceeded"),
        "response.error.code must be read: {err:?}"
    );
}

/// responses/response/incomplete_details
#[test]
fn responses_response_incomplete_details_carried() {
    let out = write_response(&mk_response(
        vec![text_block("partial")],
        Some(crate::codec::ir::IrStopReason::MaxTokens),
    ));
    assert_eq!(
        out["status"], "incomplete",
        "status must be incomplete: {out}"
    );
    assert_eq!(
        out["incomplete_details"]["reason"], "max_output_tokens",
        "incomplete_details.reason must be emitted: {out}"
    );

    let body = serde_json::json!({
        "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" },
        "output": [],
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let ir = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::MaxTokens),
        "incomplete_details.reason must be read: {ir:?}"
    );
}

/// responses/response/usage.input_tokens_details.cached_tokens
#[test]
fn responses_response_cached_tokens_survive_roundtrip() {
    let body = serde_json::json!({
        "status": "completed",
        "output": [],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "input_tokens_details": { "cached_tokens": 40 },
            "output_tokens_details": { "reasoning_tokens": 0 }
        }
    });
    let ir = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.usage.cache_read_input_tokens,
        Some(40),
        "cached_tokens must read into cache_read_input_tokens: {ir:?}"
    );

    let out = write_response(&ir);
    assert_eq!(
        out["usage"]["input_tokens_details"]["cached_tokens"], 40,
        "cached_tokens must re-emit: {out}"
    );

    assert_eq!(
        out["usage"]["input_tokens"], 100,
        "input_tokens total must include the cached prefix: {out}"
    );
}

/// responses/response/output[].type={message.id,message.status,message.content,reasoning.summary,
#[test]
fn responses_response_output_items_emitted() {
    let content = vec![
        text_block("answer"),
        crate::codec::ir::IrBlock::ToolUse {
            id: "call_z".to_string(),
            name: "search".to_string(),
            input: serde_json::json!({ "q": "rust" }),
            cache_control: None,
            thought_signature: None,
        },
        crate::codec::ir::IrBlock::Thinking {
            text: "reasoned".to_string(),
            signature: Some("ENC".to_string()),
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin: None,
        },
    ];
    let out = write_response(&mk_response(
        content,
        Some(crate::codec::ir::IrStopReason::ToolUse),
    ));
    let output = out["output"].as_array().expect("output array");

    let msg = output
        .iter()
        .find(|it| it["type"] == "message")
        .expect("message item");
    assert!(
        msg["id"].as_str().is_some_and(|s| s.starts_with("msg")),
        "message.id must be emitted: {out}"
    );
    assert_eq!(msg["status"], "completed", "message.status: {out}");
    assert_eq!(
        msg["content"][0]["text"], "answer",
        "message.content must be emitted: {out}"
    );

    let fc = output
        .iter()
        .find(|it| it["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(fc["call_id"], "call_z", "function_call.call_id: {out}");
    assert_eq!(fc["name"], "search", "function_call.name: {out}");
    assert!(
        fc["arguments"].as_str().is_some_and(|s| s.contains("rust")),
        "function_call.arguments: {out}"
    );

    let rs = output
        .iter()
        .find(|it| it["type"] == "reasoning")
        .expect("reasoning item");
    assert!(
        rs["summary"].is_array(),
        "reasoning.summary must be present (array): {out}"
    );
    assert_eq!(
        rs["encrypted_content"], "ENC",
        "reasoning.encrypted_content must survive: {out}"
    );
}

/// responses/response/output[].type=file_search_call — a hosted-tool invocation record with no
#[test]
fn responses_response_hosted_tool_item_dropped_with_warn() {
    let body = serde_json::json!({
        "status": "completed",
        "output": [
            { "type": "file_search_call", "id": "fs_1", "status": "completed" },
            { "type": "web_search_call", "id": "ws_1", "status": "completed" },
            { "type": "message", "role": "assistant",
              "content": [{ "type": "output_text", "text": "kept", "annotations": [] }] }
        ],
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });

    let seam = crate::codec::drops::Seam {
        direction: crate::codec::drops::Direction::Response,
        ingress: "openai",
        egress: "responses",
    };
    let ((ir, cap), dropped) = crate::codec::drops::scope(seam, || {
        with_warns(|| {
            ResponsesReader
                .read_response(crate::codec::test_host::HOST, &body)
                .expect("read_response")
        })
    });
    assert_eq!(dropped, vec!["output[].type=file_search_call".to_string()]);

    assert!(
        ir.content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Text { text, .. } if text == "kept")),
        "the assistant message must survive even as file_search_call drops: {ir:?}"
    );
    assert!(
        ir.content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::HostedToolRecord { .. })),
        "a web_search_call is carried as the hosted web-search record: {ir:?}"
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("file_search_call")),
        "dropping a file_search_call output item must warn, naming the type: {:?}",
        diagnostic_texts(&cap)
    );
}

/// responses/response/{instructions,metadata} — the request-echo members a native response carries
#[test]
fn responses_response_instructions_metadata_dropped_with_warn() {
    let body = serde_json::json!({
        "status": "completed",
        "instructions": "system prompt echo",
        "metadata": { "trace": "abc" },
        "output": [
            { "type": "message", "role": "assistant",
              "content": [{ "type": "output_text", "text": "hi", "annotations": [] }] }
        ],
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let (_ir, cap) = with_warns(|| {
        ResponsesReader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read_response")
    });

    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("instructions")),
        "dropping the response instructions echo must warn: {:?}",
        diagnostic_texts(&cap)
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("metadata")),
        "dropping the response metadata echo must warn: {:?}",
        diagnostic_texts(&cap)
    );
}

/// responses/response/stream:{response.created,response.output_item.added,response.output_item.done,
#[test]
fn responses_stream_events_emitted_by_writer() {
    let w = ResponsesWriter;
    let mut seen: Vec<String> = Vec::new();

    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_s".to_string()),
            created: Some(1_700_000_000),
            model: Some("gpt-s".to_string()),
        },
    ));

    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    ));
    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
    ));
    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    ));

    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 1,
            block: crate::codec::ir::IrBlockMeta::ToolUse {
                id: "call_s".to_string(),
                name: "f".to_string(),
            },
            refusal: false,
        },
    ));
    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 1,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("{}".to_string()),
        },
    ));
    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::BlockStop { index: 1 },
    ));
    seen.extend(event_names(
        &w,
        crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage(1, 1),
            stop_detail: None,
        },
    ));

    for expected in [
        EVT_RESPONSE_CREATED,
        EVT_OUTPUT_ITEM_ADDED,
        EVT_OUTPUT_ITEM_DONE,
        EVT_CONTENT_PART_ADDED,
        EVT_CONTENT_PART_DONE,
        EVT_OUTPUT_TEXT_DELTA,
        EVT_OUTPUT_TEXT_DONE,
        EVT_FUNCTION_CALL_ARGS_DELTA,
        EVT_RESPONSE_COMPLETED,
    ] {
        assert!(
            seen.iter().any(|n| n == expected),
            "stream event {expected} must be emitted; saw {seen:?}"
        );
    }
}

/// A duplicate `MessageStart` must not restart the stream. Five of the six readers gate
#[test]
fn responses_duplicate_message_start_does_not_restart_stream() {
    let w = ResponsesWriter;

    let start = |id: &str| crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: Some(id.to_string()),
        created: Some(1_700_000_000),
        model: Some("gpt-dup".to_string()),
    };

    let mut frames: Vec<(String, serde_json::Value)> = Vec::new();
    frames.extend(w.write_response_events(crate::codec::test_host::HOST, &start("resp_first")));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    ));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
    ));

    frames.extend(w.write_response_events(crate::codec::test_host::HOST, &start("resp_second")));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    ));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage(1, 1),
            stop_detail: None,
        },
    ));

    let seqs: Vec<u64> = frames
        .iter()
        .filter_map(|(_, d)| d["sequence_number"].as_u64())
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "sequence_number must be strictly increasing across the whole stream: {seqs:?}"
    );

    let created_id = frames
        .iter()
        .find(|(n, _)| n == EVT_RESPONSE_CREATED)
        .map(|(_, d)| d["response"]["id"].clone())
        .expect("response.created must be emitted");
    let completed_id = frames
        .iter()
        .find(|(n, _)| n == EVT_RESPONSE_COMPLETED)
        .map(|(_, d)| d["response"]["id"].clone())
        .expect("response.completed must be emitted");
    assert_eq!(
        created_id,
        serde_json::json!("resp_first"),
        "the stream's id is the one its FIRST opening event carried, not the duplicate's"
    );
    assert_eq!(
        created_id, completed_id,
        "the terminal event must replay the id response.created carried"
    );

    for (_, d) in frames.iter().filter(|(n, _)| n == EVT_RESPONSE_CREATED) {
        assert_eq!(
            d["response"]["id"],
            serde_json::json!("resp_first"),
            "every response.created in one stream states one id: {d}"
        );
    }

    assert!(
        frames.iter().any(|(n, _)| n == EVT_OUTPUT_TEXT_DONE),
        "the text item opened before the duplicate must still close: {:?}",
        frames.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// The per-stream reset clears every accumulator a reused or cloned writer could otherwise leak
#[test]
fn responses_reset_clears_citation_and_logprob_accumulators() {
    let w = ResponsesWriter;

    w.append_citations(
        0,
        &[crate::codec::ir::IrCitation {
            domain: None,
            kind: Some("web_search_result_location".to_string()),
            cited_text: None,
            title: Some("stale".to_string()),
            url: Some("https://stale".to_string()),
            document_index: None,
            start_index: Some(0),
            end_index: Some(2),
            encrypted_index: None,
            raw: None,
            ..Default::default()
        }],
    );
    w.append_logprobs(
        0,
        &[crate::codec::ir::IrTokenLogprob {
            token: "stale".into(),
            logprob: -0.25,
            bytes: None,
            bytes_null: false,
            top: Vec::new(),
        }],
    );

    let mut frames: Vec<(String, serde_json::Value)> = w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_reset".to_string()),
            created: Some(1_700_000_000),
            model: Some("gpt-reset".to_string()),
        },
    );
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    ));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("fresh".to_string()),
        },
    ));
    frames.extend(w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    ));

    for (name, data) in &frames {
        let blob = data.to_string();
        assert!(
            !blob.contains("https://stale"),
            "a previous stream's citation must not reach {name}: {data}"
        );
        assert!(
            !blob.contains("\"stale\""),
            "a previous stream's logprob token must not reach {name}: {data}"
        );
    }
}

/// responses/response/stream:response.incomplete
#[test]
fn responses_stream_incomplete_event_emitted() {
    let w = ResponsesWriter;
    let _ = event_names(
        &w,
        crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_i".to_string()),
            created: Some(1),
            model: Some("m".to_string()),
        },
    );
    let names = event_names(
        &w,
        crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::MaxTokens),
            stop_sequence: None,
            usage: usage(1, 1),
            stop_detail: None,
        },
    );
    assert!(
        names.iter().any(|n| n == EVT_RESPONSE_INCOMPLETE),
        "a truncated terminal must emit response.incomplete: {names:?}"
    );
}

/// responses/response/stream:response.failed
#[test]
fn responses_stream_failed_event_emitted() {
    let w = ResponsesWriter;
    let names = event_names(
        &w,
        crate::codec::ir::IrStreamEvent::Error(busbar_contract::protocol::IrError {
            class: busbar_contract::upstream::StatusClass::ServerError,
            provider_signal: Some("boom".to_string()),
            retry_after: None,
        }),
    );
    assert!(
        names.iter().any(|n| n == EVT_RESPONSE_FAILED),
        "an Error event must emit response.failed: {names:?}"
    );
}

/// responses/response/stream:response.in_progress — the reader CONSUMES the in-progress lifecycle
#[test]
fn responses_stream_in_progress_consumed_by_reader() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let data = serde_json::json!({
        "response": { "id": "resp_p", "created_at": 1, "model": "m" }
    });
    let events = ResponsesReader.read_response_events(
        crate::codec::test_host::HOST,
        "response.in_progress",
        &data,
        &mut state,
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, crate::codec::ir::IrStreamEvent::MessageStart { .. })),
        "response.in_progress must open the message: {events:?}"
    );
}

/// responses/response/stream:response.reasoning_summary_text.delta — the reader CONSUMES the
#[test]
fn responses_stream_reasoning_summary_delta_consumed_by_reader() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let data = serde_json::json!({ "output_index": 0, "delta": "musing" });
    let events = ResponsesReader.read_response_events(
        crate::codec::test_host::HOST,
        "response.reasoning_summary_text.delta",
        &data,
        &mut state,
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::ThinkingDelta(t),
                ..
            } if t == "musing"
        )),
        "reasoning_summary_text.delta must route to a ThinkingDelta: {events:?}"
    );
}

#[test]
fn top_logprobs_carries_responses_roundtrip_and_cross_to_openai() {
    let body = serde_json::json!({"model": "x", "input": "hi", "top_logprobs": 5});

    let rt = roundtrip_req(&body);
    assert_eq!(
        rt.get("top_logprobs"),
        Some(&serde_json::json!(5)),
        "top_logprobs must round-trip on the Responses surface: {rt}"
    );

    let ir = read_req(&body);
    assert_eq!(
        ir.top_logprobs,
        Some(5),
        "IR must carry the top_logprobs ask"
    );
    let openai = crate::codec::openai_chat::openai_writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        openai.get("top_logprobs"),
        Some(&serde_json::json!(5)),
        "top_logprobs must survive the cross-protocol hop to OpenAI Chat: {openai}"
    );
    assert_eq!(
        openai.get("logprobs"),
        Some(&serde_json::json!(true)),
        "the OpenAI writer must force the enabling logprobs flag: {openai}"
    );
}

#[test]
fn responses_drops_penalties_seed_n_observably() {
    let ir = crate::codec::ir::IrRequest {
        frequency_penalty: Some(0.5),
        presence_penalty: Some(0.25),
        seed: Some(42),
        n: Some(3),
        ..Default::default()
    };
    let (_out, cap) = with_warns(|| write_req(&ir));
    for field in ["frequency_penalty", "presence_penalty", "seed", "n"] {
        assert!(
            diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains(field)),
            "dropping {field} on Responses egress must warn: {:?}",
            diagnostic_texts(&cap)
        );
    }

    let w = ResponsesWriter;
    let dropped = w.dropped_egress_controls(&ir);
    for field in ["frequency_penalty", "presence_penalty", "seed", "n"] {
        assert!(
            dropped.contains(&field),
            "{field} must be reported by dropped_egress_controls: {dropped:?}"
        );
    }
}

/// responses/request/{previous_response_id,store} — the server-side conversation-state knobs a proxy
#[test]
fn responses_stateful_previous_response_id_and_store_survive_same_protocol() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [{ "type": "input_text", "text": "continue" }],
        "previous_response_id": "resp_prev_9f8e7d",
        "store": true,
    });
    let out = roundtrip_req(&body);
    assert_eq!(
        out["previous_response_id"], "resp_prev_9f8e7d",
        "previous_response_id must round-trip verbatim: {out}"
    );
    assert_eq!(out["store"], true, "store:true must round-trip: {out}");

    let body_false = serde_json::json!({
        "model": "gpt-4o",
        "input": "x",
        "store": false,
    });
    let out_false = roundtrip_req(&body_false);
    assert_eq!(
        out_false["store"], false,
        "store:false must round-trip as false: {out_false}"
    );
}

/// responses/response/id — the STORED-RESPONSE id echo / `response.id` correlation. When a client uses
#[test]
fn responses_stateful_response_id_correlation_round_trips() {
    let upstream = serde_json::json!({
        "id": "resp_stored_abc123",
        "object": "response",
        "created_at": 1_700_000_500u64,
        "model": "gpt-4o",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "ok" }],
        }],
        "usage": { "input_tokens": 3, "output_tokens": 1 },
    });
    let ir = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &upstream)
        .expect("read_response ok");
    assert_eq!(
        ir.id.as_deref(),
        Some("resp_stored_abc123"),
        "the upstream stored-response id must be captured into IR for correlation"
    );
    let out = write_response(&ir);
    assert_eq!(
        out["id"], "resp_stored_abc123",
        "the stored-response id must be echoed verbatim so the client's linkage survives: {out}"
    );
}
