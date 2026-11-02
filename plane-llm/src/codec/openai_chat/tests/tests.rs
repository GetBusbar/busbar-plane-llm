use super::*;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

#[test]
fn buffered_annotations_index_content_without_refusal_text() {
    let citation = crate::codec::ir::IrCitation {
        url: Some("https://example.com/source".into()),
        title: Some("Source".into()),
        start_index: Some(0),
        end_index: Some(2),
        ..Default::default()
    };
    let response = crate::codec::ir::IrResponse {
        content: vec![
            IrBlock::Text {
                text: "refused".into(),
                refusal: true,
                cache_control: None,
                citations: vec![citation.clone()],
            },
            IrBlock::text("é "),
            IrBlock::Text {
                text: "ok".into(),
                refusal: false,
                cache_control: None,
                citations: vec![citation],
            },
        ],
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &response);
    let message = &out["choices"][0]["message"];
    assert_eq!(message["content"], "é ok");
    assert_eq!(message["refusal"], "refused");
    let annotations = message["annotations"].as_array().expect("annotations");
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0]["url_citation"]["start_index"], 2);
    assert_eq!(annotations[0]["url_citation"]["end_index"], 4);
}
use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrMessage, IrRole, IrStreamEvent, IrUsage};

/// The streaming `include_usage` trailer must be its OWN chunk, not folded onto the finish
#[test]
fn test_split_openai_trailing_usage_unfolds_finish_chunk() {
    let mut finish = serde_json::json!({
        "id": "chatcmpl-abc",
        "object": OBJ_CHUNK,
        "created": 1234,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_STOP}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    });
    let trailing =
        split_openai_trailing_usage(&mut finish).expect("a usage-bearing finish chunk must split");

    assert!(
        finish.get("usage").is_none(),
        "usage must be lifted OFF the finish chunk: {finish}"
    );
    assert_eq!(
        finish.pointer("/choices/0/finish_reason"),
        Some(&serde_json::json!(FINISH_STOP)),
        "finish chunk keeps its finish_reason"
    );

    assert_eq!(trailing.get("id"), Some(&serde_json::json!("chatcmpl-abc")));
    assert_eq!(trailing.get("created"), Some(&serde_json::json!(1234)));
    assert_eq!(trailing.get("model"), Some(&serde_json::json!("gpt-4o")));
    assert_eq!(
        trailing.get("object"),
        Some(&serde_json::json!("chat.completion.chunk")) // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(
        trailing.get("choices"),
        Some(&serde_json::json!([])),
        "trailing usage chunk carries an EMPTY choices array (native shape)"
    );
    assert_eq!(
        trailing.get("usage"),
        Some(&serde_json::json!({"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}))
    );
}

/// A chunk WITHOUT a folded usage (every non-finish chunk, and a finish chunk that never
#[test]
fn test_split_openai_trailing_usage_noop_without_usage() {
    let mut finish = serde_json::json!({
        "object": OBJ_CHUNK,
        "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_STOP}]
    });
    assert!(
        split_openai_trailing_usage(&mut finish).is_none(),
        "no usage → no split"
    );

    let mut other = serde_json::json!({
        "object": "error",
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    assert!(
        split_openai_trailing_usage(&mut other).is_none(),
        "a non-chunk body is not split"
    );
    assert!(other.get("usage").is_some(), "non-chunk left untouched");
}

/// The writer folds usage onto the terminal `MessageDelta` whenever the counts are nonzero — NOT
#[test]
fn test_folded_usage_without_a_finish_reason_still_unfolds() {
    let writer = OpenAiWriter;
    let (_, mut chunk) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: None,
                stop_sequence: None,
                usage: IrUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    detail: crate::codec::ir::IrUsageDetail::default(),
                },
                stop_detail: None,
            },
        )
        .expect("a terminal MessageDelta with counts writes a chunk");
    assert!(
        chunk.get("usage").is_some(),
        "the writer folds usage onto this chunk even with no stop reason: {chunk}"
    );
    assert!(
        chunk
            .pointer("/choices/0/finish_reason")
            .is_some_and(serde_json::Value::is_null),
        "and its finish_reason is null: {chunk}"
    );

    let mut opted_in = chunk.clone();
    let trailing = split_openai_trailing_usage(&mut opted_in)
        .expect("a folded usage un-folds whether or not the chunk carries a finish_reason");
    assert!(
        opted_in.get("usage").is_none(),
        "usage is lifted OFF the source chunk: {opted_in}"
    );
    assert_eq!(
        trailing.get("choices"),
        Some(&serde_json::json!([])),
        "the trailing chunk carries an EMPTY choices array, as the spec declares: {trailing}"
    );
    assert_eq!(
        trailing.pointer("/usage/prompt_tokens"),
        Some(&serde_json::json!(7)),
        "the counts survive the hop: {trailing}"
    );

    strip_folded_usage(&mut chunk);
    assert!(
        chunk.get("usage").is_none(),
        "a client that did not ask for usage is not sent it: {chunk}"
    );
}

fn text_block(text: &str) -> IrBlock {
    IrBlock::text(text.to_string())
}

#[test]
fn stream_tool_use_block_start_uses_ir_index() {
    let w = OpenAiWriter;
    let ev = IrStreamEvent::BlockStart {
        index: 2,
        block: IrBlockMeta::ToolUse {
            id: "call_b".to_string(),
            name: "lookup".to_string(),
        },
        refusal: false,
    };
    let (_, chunk) = w
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("tool-use start emits a chunk");
    let tc = &chunk["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["index"], serde_json::json!(2));
    assert_eq!(tc["id"], serde_json::json!("call_b"));
    assert_eq!(tc["function"]["name"], serde_json::json!("lookup"));
}

#[test]
fn stream_input_json_delta_uses_ir_index() {
    let w = OpenAiWriter;
    let ev = IrStreamEvent::BlockDelta {
        index: 3,
        delta: IrDelta::InputJsonDelta("{\"q\":1}".to_string()),
    };
    let (_, chunk) = w
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("json delta emits a chunk");
    let tc = &chunk["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["index"], serde_json::json!(3));
    assert_eq!(tc["function"]["arguments"], serde_json::json!("{\"q\":1}"));
}

#[test]
fn stream_parallel_tool_calls_do_not_collide_at_index_zero() {
    let w = OpenAiWriter;
    let mk_start = |idx: usize, id: &str| IrStreamEvent::BlockStart {
        index: idx,
        block: IrBlockMeta::ToolUse {
            id: id.to_string(),
            name: "f".to_string(),
        },
        refusal: false,
    };
    let mk_delta = |idx: usize, frag: &str| IrStreamEvent::BlockDelta {
        index: idx,
        delta: IrDelta::InputJsonDelta(frag.to_string()),
    };

    let s1 = w
        .write_response_event(crate::codec::test_host::HOST, &mk_start(1, "a"))
        .unwrap()
        .1;
    let s2 = w
        .write_response_event(crate::codec::test_host::HOST, &mk_start(2, "b"))
        .unwrap()
        .1;
    let d1 = w
        .write_response_event(crate::codec::test_host::HOST, &mk_delta(1, "x"))
        .unwrap()
        .1;
    let d2 = w
        .write_response_event(crate::codec::test_host::HOST, &mk_delta(2, "y"))
        .unwrap()
        .1;

    let idx = |v: &serde_json::Value| v["choices"][0]["delta"]["tool_calls"][0]["index"].clone();

    assert_ne!(idx(&s1), idx(&s2));

    assert_eq!(idx(&s1), idx(&d1));
    assert_eq!(idx(&s2), idx(&d2));
}

#[test]
fn read_request_promotes_non_leading_system_message() {
    let body = serde_json::json!({
        "model": "gpt-x",
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "system", "content": "be terse" },
            { "role": "assistant", "content": "ok" }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");

    assert_eq!(ir.system.len(), 1);
    assert_eq!(ir.system[0], text_block("be terse"));

    assert!(ir.messages.iter().all(|m| m.role != IrRole::System));
    assert_eq!(ir.messages.len(), 2);
}

#[test]
fn read_request_concatenates_multiple_system_messages() {
    let body = serde_json::json!({
        "messages": [
            { "role": "system", "content": "first" },
            { "role": "user", "content": "hi" },
            { "role": "system", "content": "second" }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.system, vec![text_block("first"), text_block("second")]);
    assert!(ir.messages.iter().all(|m| m.role != IrRole::System));
}

#[test]
fn read_request_preserves_contentless_system_message() {
    let body = serde_json::json!({
        "messages": [
            { "role": "system" },
            { "role": "user", "content": "hi" }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.system, vec![text_block("")]);
}

#[test]
fn read_request_preserves_empty_array_system_message() {
    let body = serde_json::json!({
        "messages": [
            { "role": "system", "content": [] },
            { "role": "user", "content": "hi" }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.system, vec![text_block("")]);
}

#[test]
fn read_request_developer_role_feeds_system_not_rejected() {
    let body = serde_json::json!({
        "model": "o3",
        "messages": [
            { "role": "developer", "content": "be precise" },
            { "role": "user", "content": "hi" }
        ]
    });

    let ir = OpenAiReader
        .read_request(&body)
        .expect("developer role must not 400");

    assert_eq!(ir.system, vec![text_block("be precise")]);

    assert!(ir.messages.iter().all(|m| m.role != IrRole::System));
}

#[test]
fn read_request_promotes_max_completion_tokens_into_ir() {
    let body = serde_json::json!({
        "model": "o3",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_completion_tokens": 256
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.max_tokens, Some(256));

    assert!(!ir.extra.contains_key("max_completion_tokens"));
}

#[test]
fn read_request_prefers_max_tokens_over_max_completion_tokens() {
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 100,
        "max_completion_tokens": 999
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.max_tokens, Some(100));
    assert!(!ir.extra.contains_key("max_tokens"));
    assert!(!ir.extra.contains_key("max_completion_tokens"));
}

#[test]
fn read_request_ignores_nonpositive_max_completion_tokens() {
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "max_completion_tokens": 0
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.max_tokens, None);
}

/// Regression: a ToolResult whose content is multiple Text blocks
#[test]
fn write_request_tool_result_multi_text_concatenates_without_separator() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Tool,
            content: vec![crate::codec::ir::IrBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: vec![text_block("AAA"), text_block("BBB")],
                is_error: false,
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let tool_msg = out["messages"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["role"] == "tool"))
        .expect("a tool-role message");
    assert_eq!(
        tool_msg["content"], "AAABBB",
        "multi-text ToolResult content must concatenate with NO separator, got {}",
        tool_msg["content"]
    );
}

#[test]
fn write_request_forces_logprobs_flag_when_only_top_logprobs_present() {
    let mut ir = OpenAiReader
        .read_request(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }],
            "top_logprobs": 5
        }))
        .expect("parses");
    ir.logprobs = None;
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["logprobs"], true, "enabling flag must be forced");
    assert_eq!(out["top_logprobs"], 5);
}

#[test]
fn write_request_emits_max_tokens_from_modeled_cap() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![text_block("hi")],
        }],
        tools: Vec::new(),
        max_tokens: Some(512),
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(out["max_tokens"], serde_json::json!(512));

    assert!(out
        .as_object()
        .expect("object")
        .get("max_completion_tokens")
        .is_none());
}

#[test]
fn max_completion_tokens_survives_read_write_roundtrip() {
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "max_completion_tokens": 777
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["max_completion_tokens"],
        serde_json::json!(777),
        "same-protocol passthrough must preserve the source `max_completion_tokens` key"
    );

    assert!(
        out.as_object().expect("object").get("max_tokens").is_none(),
        "must not emit `max_tokens` alongside the preserved `max_completion_tokens`"
    );

    assert!(out
        .as_object()
        .expect("object")
        .get(MAX_COMPLETION_TOKENS_SENTINEL)
        .is_none());
}

#[test]
fn max_completion_tokens_maps_to_max_tokens_cross_protocol() {
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "max_completion_tokens": 777
    });
    let mut ir = OpenAiReader.read_request(&body).expect("parses");
    ir.extra.clear();
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["max_tokens"],
        serde_json::json!(777),
        "cross-protocol egress emits the canonical `max_tokens`"
    );
    assert!(
        out.as_object()
            .expect("object")
            .get("max_completion_tokens")
            .is_none(),
        "cross-protocol egress must not carry `max_completion_tokens`"
    );
}

#[test]
fn response_format_survives_same_protocol_roundtrip() {
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "response_format": {
            "type": RESP_FORMAT_JSON_SCHEMA,
            "json_schema": {"name": "out", "schema": {"type": "object"}}
        }
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");

    assert!(
        !ir.extra.contains_key("response_format"),
        "response_format must be promoted to the typed IR field, not left in extra"
    );
    assert_eq!(
        ir.response_format,
        Some(crate::codec::ir::IrResponseFormat {
            json: true,
            schema: Some(serde_json::json!({"type": "object"})),
            name: Some("out".to_string()),
            strict: None,
            description: None,
        })
    );
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["response_format"],
        serde_json::json!({
            "type": "json_schema", // golden wire-contract literal (kept bare on purpose)
            "json_schema": {"name": "out", "schema": {"type": "object"}}
        }),
        "response_format must round-trip on a same-protocol OpenAI passthrough"
    );
}

#[test]
fn write_request_omits_token_cap_when_absent() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![text_block("hi")],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let obj = out.as_object().expect("object");
    assert!(obj.get("max_completion_tokens").is_none());
    assert!(obj.get("max_tokens").is_none());
}

#[test]
fn write_request_keeps_tool_use_on_user_message() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![IrBlock::ToolUse {
                thought_signature: None,
                id: "t9".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({"q": "rust"}),
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages array");
    let user_msg = &msgs[0];
    let tcs = user_msg["tool_calls"]
        .as_array()
        .expect("tool_calls preserved on user message");
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0]["id"], serde_json::json!("t9"));
    assert_eq!(tcs[0]["function"]["name"], serde_json::json!("search"));
    assert_eq!(
        tcs[0]["function"]["arguments"],
        serde_json::json!("{\"q\":\"rust\"}")
    );
}

/// A Tool-role message carrying ONLY ToolResult blocks must
#[test]
fn write_request_pure_tool_result_message_emits_only_flat_entries() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Tool,
            content: vec![IrBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: vec![text_block("42")],
                is_error: false,
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages array");
    assert_eq!(
        msgs.len(),
        1,
        "pure tool-result must yield exactly one entry"
    );
    assert_eq!(msgs[0]["role"], serde_json::json!("tool"));
    assert_eq!(msgs[0]["tool_call_id"], serde_json::json!("call_1"));
    assert_eq!(msgs[0]["content"], serde_json::json!("42"));
}

/// A Tool-role message carrying BOTH a ToolResult block AND
#[test]
fn write_request_tool_role_mixed_content_not_dropped() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Tool,
            content: vec![
                IrBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: vec![text_block("result")],
                    is_error: false,
                    cache_control: None,
                },
                text_block("stray narration"),
                IrBlock::ToolUse {
                    thought_signature: None,
                    id: "call_2".to_string(),
                    name: "lookup".to_string(),
                    input: serde_json::json!({"k": "v"}),
                    cache_control: None,
                },
            ],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages array");

    assert_eq!(
        msgs.len(),
        2,
        "tool-result entry + the non-dropped mixed-content entry, got {msgs:?}"
    );

    let flat = msgs
        .iter()
        .find(|m| m.get("tool_call_id").is_some())
        .expect("flat tool-result entry present");
    assert_eq!(flat["tool_call_id"], serde_json::json!("call_1"));

    let carried = msgs
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .expect("the non-ToolResult content (text + tool_calls) must not be dropped");
    let tcs = carried["tool_calls"].as_array().expect("tool_calls array");
    assert_eq!(tcs[0]["id"], serde_json::json!("call_2"));

    let content = carried["content"]
        .as_array()
        .expect("stray text content survives as an array");
    assert!(
        content
            .iter()
            .any(|c| c["type"] == "text" && c["text"] == "stray narration"),
        "stray text must survive, got {content:?}"
    );
}

/// Regression: a Gemini `functionResponse` decodes to an IrRole::User message carrying
#[test]
fn write_request_tool_result_on_user_message_emits_tool_message() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![IrBlock::ToolResult {
                tool_use_id: "call_42".to_string(),
                content: vec![text_block("the answer is 42")],
                is_error: false,
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages array");

    assert_eq!(
        msgs.len(),
        1,
        "exactly the flat tool-result entry, got {msgs:?}"
    );
    let tool_msg = &msgs[0];
    assert_eq!(
        tool_msg["role"], "tool",
        "a ToolResult on a User-role message must become an OpenAI tool message, got {tool_msg:?}"
    );
    assert_eq!(tool_msg["tool_call_id"], serde_json::json!("call_42"));
    assert_eq!(tool_msg["content"], serde_json::json!("the answer is 42"));
}

#[test]
fn write_response_joins_text_blocks_and_keeps_tool_calls() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            text_block("Hello "),
            text_block("world"),
            IrBlock::ToolUse {
                thought_signature: None,
                id: "c1".to_string(),
                name: "fn".to_string(),
                input: serde_json::json!({"a": 1}),
                cache_control: None,
            },
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let msg = &out["choices"][0]["message"];
    assert_eq!(msg["content"], serde_json::json!("Hello world"));
    assert_eq!(msg["tool_calls"][0]["id"], serde_json::json!("c1"));
    assert_eq!(
        out["choices"][0]["finish_reason"],
        serde_json::json!("tool_calls") // golden wire-contract literal (kept bare on purpose)
    );
}

#[test]
fn write_response_content_null_when_no_text() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![IrBlock::ToolUse {
            thought_signature: None,
            id: "c1".to_string(),
            name: "fn".to_string(),
            input: serde_json::json!({}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out["choices"][0]["message"]["content"],
        serde_json::Value::Null
    );
}

#[test]
fn write_error_native_openai_shape() {
    let v = openai_writer().write_error(
        crate::codec::test_host::HOST,
        404,
        ERR_TYPE_NOT_FOUND,
        "model 'gpt-z' not found",
    );

    assert_eq!(
        v["error"]["message"],
        serde_json::json!("model 'gpt-z' not found")
    );
    assert_eq!(v["error"]["type"], serde_json::json!("not_found_error"));
    assert_eq!(v["error"]["param"], serde_json::Value::Null);
    assert_eq!(v["error"]["code"], serde_json::Value::Null);

    let s = serde_json::to_string(&v).expect("serializes");
    let re: serde_json::Value = serde_json::from_str(&s).expect("valid json");
    assert!(re.get("error").is_some());
}

#[test]
fn write_error_maps_kind_vocabulary() {
    for (kind, want) in [
        ("auth", ERR_TYPE_AUTHENTICATION),
        ("rate_limit", ERR_TYPE_RATE_LIMIT),
        ("forbidden", ERR_TYPE_PERMISSION),
        ("invalid_request", ERR_TYPE_INVALID_REQUEST),
        (
            crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH,
            ERR_TYPE_INVALID_REQUEST,
        ),
    ] {
        let v = openai_writer().write_error(crate::codec::test_host::HOST, 400, kind, "x");
        assert_eq!(v["error"]["type"], serde_json::json!(want), "kind={kind}");
    }
}

#[test]
fn write_error_empty_kind_falls_back_to_status_bucket() {
    let v5 = openai_writer().write_error(crate::codec::test_host::HOST, 503, "", "down");
    assert_eq!(v5["error"]["type"], serde_json::json!("server_error"));
    let v4 = openai_writer().write_error(crate::codec::test_host::HOST, 400, "", "bad");
    assert_eq!(
        v4["error"]["type"],
        serde_json::json!("invalid_request_error") // golden wire-contract literal (kept bare on purpose)
    );
}

#[test]
fn read_response_captures_upstream_identity() {
    let body = serde_json::json!({
        "id": "chatcmpl-abc123",
        "object": OBJ_COMPLETION,
        "created": 1_700_000_000u64,
        "model": "gpt-4o",
        "system_fingerprint": "fp_deadbeef",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(ir.id.as_deref(), Some("chatcmpl-abc123"));
    assert_eq!(ir.created, Some(1_700_000_000));
    assert_eq!(ir.model.as_deref(), Some("gpt-4o"));
    assert_eq!(ir.system_fingerprint.as_deref(), Some("fp_deadbeef"));
}

#[test]
fn same_protocol_roundtrip_preserves_identity() {
    let body = serde_json::json!({
        "id": "chatcmpl-xyz789",
        "object": OBJ_COMPLETION,
        "created": 1_711_111_111u64,
        "model": "gpt-4o-mini",
        "system_fingerprint": "fp_cafef00d",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pong"},
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["id"], serde_json::json!("chatcmpl-xyz789"));
    assert_eq!(out["object"], serde_json::json!("chat.completion"));
    assert_eq!(out["created"], serde_json::json!(1_711_111_111u64));
    assert_eq!(out["model"], serde_json::json!("gpt-4o-mini"));
    assert_eq!(out["system_fingerprint"], serde_json::json!("fp_cafef00d"));

    assert_eq!(out["usage"]["total_tokens"], serde_json::json!(12));
}

#[test]
fn cross_protocol_write_synthesizes_valid_id() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hello")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let id = out["id"].as_str().expect("synthesized id is a string");
    assert!(
        id.starts_with("chatcmpl-"), // golden wire-contract literal (kept bare on purpose)
        "synthesized id has the right prefix: {id}"
    );
    assert!(
        id.len() > "chatcmpl-".len(), // golden wire-contract literal (kept bare on purpose)
        "synthesized id has a token body"
    );
    assert!(
        out["created"].as_u64().is_some(),
        "created synthesized as unix secs"
    );

    assert!(out.get("system_fingerprint").is_none());
}

#[test]
fn cross_protocol_write_response_emits_fallback_model() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let obj = out.as_object().expect("response object");
    assert!(
        obj.contains_key("model"),
        "model key must always be present"
    );
    let model = out["model"].as_str().expect("model is a non-null string");
    assert!(!model.is_empty(), "model fallback is non-empty: {model}");
    assert_eq!(out["model"], serde_json::json!(DEFAULT_MODEL));
}

#[test]
fn write_response_preserves_upstream_model_over_fallback() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o-mini".to_string()),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(out["model"], serde_json::json!("gpt-4o-mini"));
}

#[test]
fn stream_message_start_emits_fallback_model_when_none() {
    let no_model = IrStreamEvent::MessageStart {
        role: IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &no_model)
        .expect("message start emits a chunk");
    let obj = chunk.as_object().expect("chunk object");
    assert!(
        obj.contains_key("model"),
        "model key must always be present"
    );
    let model = chunk["model"].as_str().expect("model is a non-null string");
    assert!(!model.is_empty(), "model fallback is non-empty: {model}");
    assert_eq!(chunk["model"], serde_json::json!(DEFAULT_MODEL));
}

#[test]
fn stream_message_start_preserves_upstream_model_over_fallback() {
    let with_model = IrStreamEvent::MessageStart {
        role: IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: Some("gpt-4o-2024-08-06".to_string()),
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &with_model)
        .expect("message start emits a chunk");
    assert_eq!(chunk["model"], serde_json::json!("gpt-4o-2024-08-06"));
}

#[test]
fn stream_message_start_emits_identity() {
    let with_id = IrStreamEvent::MessageStart {
        role: IrRole::Assistant,
        usage: None,
        id: Some("chatcmpl-stream1".to_string()),
        created: Some(1_722_222_222),
        model: Some("gpt-4o".to_string()),
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &with_id)
        .expect("message start emits a chunk");
    assert_eq!(chunk["id"], serde_json::json!("chatcmpl-stream1"));
    assert_eq!(chunk["object"], serde_json::json!("chat.completion.chunk"));
    assert_eq!(chunk["created"], serde_json::json!(1_722_222_222u64));
    assert_eq!(chunk["model"], serde_json::json!("gpt-4o"));

    let no_id = IrStreamEvent::MessageStart {
        role: IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let (_, chunk2) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &no_id)
        .expect("message start emits a chunk");
    assert!(chunk2["id"]
        .as_str()
        .map(|s| s.starts_with("chatcmpl-")) // golden wire-contract literal (kept bare on purpose)
        .unwrap_or(false));
    assert!(chunk2["created"].as_u64().is_some());
}

#[test]
fn stream_read_captures_chunk_identity() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-stream9",
            "object": OBJ_CHUNK,
            "created": 1_733_333_333u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        }),
        &mut st,
    );
    let start = ev
        .iter()
        .find(|e| matches!(e, IrStreamEvent::MessageStart { .. }))
        .expect("MessageStart emitted");
    match start {
        IrStreamEvent::MessageStart {
            id, created, model, ..
        } => {
            assert_eq!(id.as_deref(), Some("chatcmpl-stream9"));
            assert_eq!(*created, Some(1_733_333_333));
            assert_eq!(model.as_deref(), Some("gpt-4o"));
        }
        _ => unreachable!(),
    }
}

#[test]
fn text_then_tool_closes_text_block_before_opening_tool() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let mut events = Vec::new();

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Let me check. "},
                         "finish_reason": null}]
        }),
        &mut st,
    ));

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{}"}
            }]}, "finish_reason": null}]
        }),
        &mut st,
    ));
    let tool_start = events
        .iter()
        .position(|e| {
            matches!(
                e,
                IrStreamEvent::BlockStart {
                    block: IrBlockMeta::ToolUse { .. },
                    ..
                }
            )
        })
        .expect("a tool BlockStart must be emitted");
    assert!(
        events[..tool_start]
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { .. })),
        "the open text block must be closed (BlockStop) BEFORE the tool block opens: {events:?}"
    );
}

#[test]
fn preamble_text_then_tool_then_text_opens_a_fresh_balanced_text_block() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let mut events = Vec::new();

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Thinking… "},
                         "finish_reason": null}]
        }),
        &mut st,
    ));

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{}"}
            }]}, "finish_reason": null}]
        }),
        &mut st,
    ));

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "…done."}, "finish_reason": null}]
        }),
        &mut st,
    ));

    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        }),
        &mut st,
    ));

    let text_starts: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Text,
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .collect();

    assert_eq!(
        text_starts.len(),
        2,
        "resumed text opens one new text block: {events:?}"
    );
    assert_ne!(
        text_starts[0], text_starts[1],
        "the resumed text block must not reopen the closed index: {events:?}"
    );

    for text_idx in text_starts {
        let text_stops = events
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::BlockStop { index } if *index == text_idx))
            .count();
        assert_eq!(
            text_stops, 1,
            "text block {text_idx} must be closed exactly once (balanced): {events:?}"
        );
    }
}

#[test]
fn write_response_total_tokens_saturates_on_overflow() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("x")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: u64::MAX,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };

    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(out["usage"]["total_tokens"], serde_json::json!(u64::MAX));
}

#[test]
fn read_request_preserves_sampling_params_in_extra() {
    let body = serde_json::json!({
        "model": "gpt-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "top_p": 0.9,
        "frequency_penalty": 0.5,
        "presence_penalty": 0.25,
        "stop": ["\n\n"],
        "n": 2,
        "logit_bias": { "50256": -100 }
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");

    assert!(!ir.extra.contains_key("top_p"));
    assert!(!ir.extra.contains_key("stop"));
    assert_eq!(ir.top_p, Some(0.9_f64));
    assert_eq!(ir.stop, vec!["\n\n".to_string()]);

    assert!(!ir.extra.contains_key("frequency_penalty"));
    assert!(!ir.extra.contains_key("presence_penalty"));
    assert!(!ir.extra.contains_key("n"));
    assert_eq!(ir.frequency_penalty, Some(0.5_f64));
    assert_eq!(ir.presence_penalty, Some(0.25_f64));
    assert_eq!(ir.n, Some(2));
    assert_eq!(
        ir.extra.get("logit_bias"),
        Some(&serde_json::json!({ "50256": -100 }))
    );

    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["top_p"], serde_json::json!(0.9));
    assert_eq!(out["stop"], serde_json::json!(["\n\n"]));
    assert_eq!(out["frequency_penalty"], serde_json::json!(0.5));
    assert_eq!(out["n"], serde_json::json!(2));
}

/// `reasoning_effort` values the IR has no word for (a spelling newer than this build) must NOT be
#[test]
fn unknown_reasoning_effort_survives_in_extra() {
    let body = serde_json::json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hi" }],
        "reasoning_effort": "ultra"
    });

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let ir = tracing::dispatcher::with_default(&subscriber, || {
        OpenAiReader.read_request(&body).expect("parses")
    });

    assert_eq!(
        ir.reasoning, None,
        "an unrecognised value carries no typed reasoning ask"
    );
    assert_eq!(
        ir.extra.get("reasoning_effort"),
        Some(&serde_json::json!("ultra")),
        "the raw value must survive in extra so it is not silently lost: {:?}",
        ir.extra
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("ultra")),
        "the unrecognised value must be warned about: {:?}",
        diagnostic_texts(&cap)
    );

    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["reasoning_effort"], serde_json::json!("ultra"));
}

#[test]
fn write_request_tool_call_only_assistant_has_null_content() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Assistant,
            content: vec![IrBlock::ToolUse {
                thought_signature: None,
                id: "t1".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({"q": "x"}),
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msg = &out["messages"][0];
    assert_eq!(msg["content"], serde_json::Value::Null);
    assert_eq!(msg["tool_calls"][0]["id"], serde_json::json!("t1"));
}

#[test]
fn read_block_data_uri_splits_media_type_and_payload() {
    let block = serde_json::json!({
        "type": "image_url",
        "image_url": { "url": "data:image/png;base64,AAAB" }
    });
    let ir = read_openai_block(&block).expect("parses");
    match ir {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
            ..
        } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, "AAAB");
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn read_block_https_url_kept_verbatim_with_sentinel() {
    let block = serde_json::json!({
        "type": "image_url",
        "image_url": { "url": "https://example.com/cat.png" }
    });
    let ir = read_openai_block(&block).expect("parses");
    match ir {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Url(url),
            ..
        } => {
            assert_eq!(url, "https://example.com/cat.png");
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn image_url_round_trips_through_writer() {
    for url in ["data:image/png;base64,AAAB", "https://example.com/cat.png"] {
        let source = super::super::ir_encode::parse_image_url(url);
        assert_eq!(
            super::super::ir_encode::image_url_from_ir(&source).as_deref(),
            Some(url)
        );
    }
}

#[test]
fn stream_error_uses_enumerated_openai_type() {
    let cases = [
        (
            busbar_contract::upstream::StatusClass::RateLimit,
            ERR_TYPE_RATE_LIMIT,
        ),
        (
            busbar_contract::upstream::StatusClass::Auth,
            ERR_TYPE_AUTHENTICATION,
        ),
        (
            busbar_contract::upstream::StatusClass::Billing,
            ERR_TYPE_INSUFFICIENT_QUOTA,
        ),
        (
            busbar_contract::upstream::StatusClass::ClientError,
            ERR_TYPE_INVALID_REQUEST,
        ),
        (
            busbar_contract::upstream::StatusClass::ContextLength,
            ERR_TYPE_INVALID_REQUEST,
        ),
        (
            busbar_contract::upstream::StatusClass::ServerError,
            ERR_TYPE_SERVER_ERROR,
        ),
        (
            busbar_contract::upstream::StatusClass::Overloaded,
            ERR_TYPE_SERVER_ERROR,
        ),
        (
            busbar_contract::upstream::StatusClass::Timeout,
            ERR_TYPE_SERVER_ERROR,
        ),
        (
            busbar_contract::upstream::StatusClass::Network,
            ERR_TYPE_SERVER_ERROR,
        ),
    ];
    for (class, want) in cases {
        let ev = IrStreamEvent::Error(busbar_contract::upstream::CanonicalSignal {
            class,
            provider_signal: Some("boom".to_string()),
            retry_after: None,
        });
        let (_, chunk) = openai_writer()
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .expect("error emits a chunk");
        assert_eq!(
            chunk["error"]["type"],
            serde_json::json!(want),
            "class={class:?}"
        );
        assert_eq!(chunk["error"]["message"], serde_json::json!("boom"));

        assert_ne!(chunk["error"]["type"], serde_json::json!("error"));
    }
}

#[test]
fn extract_error_derives_code_and_type_single_parse() {
    let body =
        br#"{"error":{"message":"nope","type":"invalid_request_error","code":"model_not_found"}}"#;
    let raw = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(raw.provider_code.as_deref(), Some("model_not_found"));
    assert_eq!(
        raw.structured_type.as_deref(),
        Some("invalid_request_error") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(raw.http_status, 400);

    let raw2 = OpenAiReader.extract_error(StatusCode::BAD_GATEWAY, b"<html>502</html>");
    assert!(raw2.provider_code.is_none());
    assert!(raw2.structured_type.is_none());
}

/// Regression: a context-length overflow signalled ONLY in the prose `message` with a
#[test]
fn extract_error_synthesizes_context_length_from_prose_message() {
    let body = br#"{"error":{"message":"This model's maximum context length is 8192 tokens, however you requested 9000 tokens. Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":null}}"#;
    let raw = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a prose-only maximum-context-length message must synthesize the canonical code"
    );
    assert_eq!(
        raw.structured_type.as_deref(),
        Some("invalid_request_error") // golden wire-contract literal (kept bare on purpose)
    );

    let body2 = br#"{"error":{"message":"too long","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let raw2 = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, body2);
    assert_eq!(
        raw2.provider_code.as_deref(),
        Some("context_length_exceeded")
    );

    let body3 = br#"{"error":{"message":"invalid value for parameter temperature","type":"invalid_request_error","code":null}}"#;
    let raw3 = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, body3);
    assert!(
        raw3.provider_code.is_none(),
        "a non-context-length 400 must not be tagged context_length_exceeded"
    );
}

/// Regression: the context-length message scan was OVER-BROAD — it ORed weak tokens
#[test]
fn extract_error_context_length_scan_is_precise_no_false_positives() {
    let quota = br#"{"error":{"message":"You have reached the maximum number of tokens allowed per day for this organization.","type":"insufficient_quota","code":null}}"#;
    let raw_quota = OpenAiReader.extract_error(StatusCode::TOO_MANY_REQUESTS, quota);
    assert!(
        raw_quota.provider_code.is_none(),
        "a token-quota rate-limit message must NOT be tagged context_length_exceeded"
    );

    let generic = br#"{"error":{"message":"The provided JWT token exceeds the allowed audience set.","type":"invalid_request_error","code":null}}"#;
    let raw_generic = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, generic);
    assert!(
        raw_generic.provider_code.is_none(),
        "an unrelated `token`+`exceeds` message must NOT be tagged context_length_exceeded"
    );

    let wrong_status = br#"{"error":{"message":"This model's maximum context length is 8192 tokens.","type":"server_error","code":null}}"#;
    let raw_wrong = OpenAiReader.extract_error(StatusCode::INTERNAL_SERVER_ERROR, wrong_status);
    assert!(
        raw_wrong.provider_code.is_none(),
        "a 5xx mentioning context length must NOT be reclassified as context_length_exceeded"
    );

    let real = br#"{"error":{"message":"This model's maximum context length is 8192 tokens, however you requested 9000 tokens.","type":"invalid_request_error","code":null}}"#;
    let raw_real = OpenAiReader.extract_error(StatusCode::BAD_REQUEST, real);
    assert_eq!(
        raw_real.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a real maximum-context-length 400 must still synthesize the canonical code"
    );

    let real_413 = br#"{"error":{"message":"Request exceeds the token limit for this model.","type":"invalid_request_error","code":null}}"#;
    let raw_413 = OpenAiReader.extract_error(StatusCode::PAYLOAD_TOO_LARGE, real_413);
    assert_eq!(
        raw_413.provider_code.as_deref(),
        Some("context_length_exceeded"),
        "a 413 with `exceeds`+`token limit` must synthesize the canonical code"
    );
}

#[test]
fn write_request_non_text_system_block_does_not_vanish_silently() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: vec![
            text_block("be terse"),
            IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "AAAB".to_string(),
                },
                cache_control: None,
                detail: None,
            },
        ],
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![text_block("hi")],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages");

    assert_eq!(msgs[0]["role"], serde_json::json!("system"));
    assert_eq!(msgs[0]["content"], serde_json::json!("be terse"));
    assert_eq!(msgs[1]["role"], serde_json::json!("system"));
    assert_eq!(msgs[1]["content"], serde_json::json!(""));
}

#[test]
fn synth_completion_id_matches_native_length_and_alphabet() {
    let id = synth_completion_id(crate::codec::test_host::HOST).expect("test entropy");
    let suffix = id
        .strip_prefix("chatcmpl-")
        .expect("synthesized id has the chatcmpl- prefix");
    assert_eq!(
        suffix.len(),
        COMPLETION_ID_TOKEN_LEN,
        "suffix is exactly the native 24-char width: {id}"
    );
    assert_eq!(id.len(), "chatcmpl-".len() + 24, "total length is 33: {id}");

    assert_eq!(id.matches('-').count(), 1, "no internal delimiter: {id}");

    assert!(
        suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
        "suffix is base62: {id}"
    );
}

#[test]
fn synth_completion_id_burst_is_unique_and_unbiased() {
    use std::collections::HashSet;
    const N: usize = 20_000;
    let mut seen = HashSet::with_capacity(N);

    let mut low_bucket: u64 = 0;
    let mut total_chars: u64 = 0;
    for _ in 0..N {
        let id = synth_completion_id(crate::codec::test_host::HOST).expect("test entropy");
        assert_eq!(
            id.len(),
            "chatcmpl-".len() + COMPLETION_ID_TOKEN_LEN, // golden wire-contract literal (kept bare on purpose)
            "{id}"
        );
        let suffix = id
            .strip_prefix("chatcmpl-")
            .expect("synthesized id carries the chatcmpl- prefix");
        assert!(
            suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
            "suffix is base62: {id}"
        );
        for b in suffix.bytes() {
            total_chars += 1;

            if (b'0'..=b'7').contains(&b) {
                low_bucket += 1;
            }
        }
        assert!(seen.insert(id.clone()), "duplicate synthesized id: {id}");
    }
    assert_eq!(seen.len(), N, "all {N} synthesized ids are unique");

    let share = low_bucket as f64 / total_chars as f64;
    assert!(
        share < 0.14,
        "char share for residues 0..=7 was {share:.4}; uniform≈0.1290, biased≈0.1563 — \
             a value at/above 0.14 indicates `byte % 62` bias regressed"
    );
}

#[test]
fn synth_completion_id_unique_even_with_identical_entropy() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let id = synth_completion_id(crate::codec::test_host::HOST).expect("test entropy");
        assert_eq!(id.len(), "chatcmpl-".len() + 24);
        assert!(seen.insert(id.clone()), "duplicate synthesized id: {id}");
    }
}

#[test]
fn stream_message_delta_none_stop_reason_serializes_null_not_empty_string() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: None,
        stop_sequence: None,
        usage: IrUsage::default(),
        stop_detail: None,
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message delta emits a chunk");
    let fr = &chunk["choices"][0]["finish_reason"];

    assert_eq!(*fr, serde_json::Value::Null);
    assert_ne!(*fr, serde_json::json!(""));
}

#[test]
fn stream_message_delta_maps_stop_reasons_to_openai_enum() {
    use crate::codec::ir::IrStopReason as S;
    let cases = [
        (Some(S::EndTurn), serde_json::json!("stop")),
        (Some(S::StopSequence), serde_json::json!("stop")),
        (Some(S::MaxTokens), serde_json::json!("length")),
        (Some(S::ToolUse), serde_json::json!("tool_calls")),
        (Some(S::Safety), serde_json::json!("content_filter")),
    ];
    for (stop_reason, want) in cases {
        let ev = IrStreamEvent::MessageDelta {
            stop_reason,
            stop_sequence: None,
            usage: IrUsage::default(),
            stop_detail: None,
        };
        let (_, chunk) = openai_writer()
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .expect("message delta emits a chunk");
        assert_eq!(
            chunk["choices"][0]["finish_reason"], want,
            "stop_reason={stop_reason:?}"
        );
    }
}

#[test]
fn write_request_assistant_tool_result_block_not_emitted_as_content() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Assistant,
            content: vec![
                text_block("answer"),
                IrBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![text_block("ignored")],
                    is_error: false,
                    cache_control: None,
                },
            ],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let msgs = out["messages"].as_array().expect("messages array");

    let assistant = msgs
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    let content = assistant["content"]
        .as_array()
        .expect("assistant content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], serde_json::json!("text"));
    assert_eq!(content[0]["text"], serde_json::json!("answer"));

    let tool_msg = msgs
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("ToolResult must surface as a flat tool message");
    assert_eq!(tool_msg["tool_call_id"], serde_json::json!("t1"));
    assert_eq!(tool_msg["content"], serde_json::json!("ignored"));
}

#[test]
fn write_request_thinking_block_dropped_from_message_content() {
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Assistant,
            content: vec![
                IrBlock::Thinking {
                    text: "secret reasoning".to_string(),
                    signature: None,
                    redacted: false,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                },
                text_block("visible"),
            ],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let content = out["messages"][0]["content"]
        .as_array()
        .expect("content array");

    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], serde_json::json!("visible"));
}

#[test]
fn read_request_array_content_reads_all_blocks() {
    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "one" },
                { "type": "text", "text": "two" }
            ]
        }]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.messages.len(), 1);
    assert_eq!(
        ir.messages[0].content,
        vec![text_block("one"), text_block("two")]
    );
}

#[test]
fn read_response_empty_string_content_yields_no_text_block() {
    let body = serde_json::json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": ""},
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 0}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert!(ir
        .content
        .iter()
        .all(|b| !matches!(b, IrBlock::Text { .. })));
}

#[test]
fn stream_trailing_usage_only_chunk_emits_message_delta_with_usage() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-u1",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]
        }),
        &mut st,
    );

    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-u1",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 7,
                "prompt_tokens_details": { "cached_tokens": 3 }
            }
        }),
        &mut st,
    );
    let delta = evs
        .iter()
        .find(|e| matches!(e, IrStreamEvent::MessageDelta { .. }))
        .expect("trailing usage chunk yields a MessageDelta");
    match delta {
        IrStreamEvent::MessageDelta {
            stop_reason,
            stop_sequence,
            usage,
            stop_detail: _,
        } => {
            assert_eq!(*stop_reason, None);
            assert_eq!(*stop_sequence, None);

            assert_eq!(usage.input_tokens, 8);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.cache_read_input_tokens, Some(3));
        }
        _ => unreachable!(),
    }

    assert!(!evs.iter().any(|e| matches!(e, IrStreamEvent::MessageStop)));
}

#[test]
fn stream_usage_on_finish_chunk_still_captured() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-u2",
            "created": 1_700_000_001u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_STOP}],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        }),
        &mut st,
    );
    let delta = evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageDelta {
                stop_reason, usage, ..
            } => Some((*stop_reason, usage.clone())),
            _ => None,
        })
        .expect("finish chunk yields a MessageDelta");
    assert_eq!(delta.0, Some(crate::codec::ir::IrStopReason::EndTurn));
    assert_eq!(delta.1.input_tokens, 5);
    assert_eq!(delta.1.output_tokens, 2);
    assert!(evs.iter().any(|e| matches!(e, IrStreamEvent::MessageStop)));
}

#[test]
fn stream_error_envelope_includes_null_code_and_param() {
    let ev = IrStreamEvent::Error(busbar_contract::upstream::CanonicalSignal {
        class: busbar_contract::upstream::StatusClass::RateLimit,
        provider_signal: Some("slow down".to_string()),
        retry_after: None,
    });
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error emits a chunk");
    assert_eq!(chunk["error"]["message"], serde_json::json!("slow down"));
    assert_eq!(
        chunk["error"]["type"],
        serde_json::json!("rate_limit_error") // golden wire-contract literal (kept bare on purpose)
    );

    assert_eq!(chunk["error"]["code"], serde_json::Value::Null);
    assert_eq!(chunk["error"]["param"], serde_json::Value::Null);

    let err_obj = chunk["error"].as_object().expect("error object");
    assert!(err_obj.contains_key("code"));
    assert!(err_obj.contains_key("param"));
}

#[test]
fn stream_error_shape_matches_write_error_shape() {
    let ev = IrStreamEvent::Error(busbar_contract::upstream::CanonicalSignal {
        class: busbar_contract::upstream::StatusClass::Auth,
        provider_signal: Some("nope".to_string()),
        retry_after: None,
    });
    let (_, stream_chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error emits a chunk");
    let non_stream =
        openai_writer().write_error(crate::codec::test_host::HOST, 401, "auth", "nope");
    let mut stream_keys: Vec<&String> = stream_chunk["error"]
        .as_object()
        .expect("stream error object")
        .keys()
        .collect();
    let mut non_stream_keys: Vec<&String> = non_stream["error"]
        .as_object()
        .expect("non-stream error object")
        .keys()
        .collect();
    stream_keys.sort();
    non_stream_keys.sort();
    assert_eq!(stream_keys, non_stream_keys);
}

#[test]
fn write_response_falls_back_to_stop_when_stop_reason_none() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("partial")],
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let choice = out["choices"][0].as_object().expect("choice object");
    assert!(
        choice.contains_key("finish_reason"),
        "finish_reason key must always be present"
    );
    assert_eq!(choice["finish_reason"], serde_json::json!("stop"));
}

#[test]
fn write_response_maps_finish_reason_enum_values() {
    use crate::codec::ir::IrStopReason as S;
    let cases = [
        (Some(S::EndTurn), serde_json::json!("stop")),
        (Some(S::StopSequence), serde_json::json!("stop")),
        (Some(S::MaxTokens), serde_json::json!("length")),
        (Some(S::ToolUse), serde_json::json!("tool_calls")),
        (Some(S::Safety), serde_json::json!("content_filter")),
        (Some(S::Refusal), serde_json::json!("stop")),
        (Some(S::Error), serde_json::json!("stop")),
        (Some(S::PauseTurn), serde_json::json!("stop")),
        (None, serde_json::json!("stop")),
    ];
    for (stop_reason, want) in cases {
        let resp = crate::codec::ir::IrResponse {
            content: vec![text_block("x")],
            stop_reason,
            ..Default::default()
        };
        let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
        assert_eq!(
            out["choices"][0]["finish_reason"], want,
            "stop_reason={stop_reason:?}"
        );
    }
}

#[test]
fn stream_tool_call_index_u64_max_does_not_panic_or_wrap() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-ov",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": u64::MAX,
                    "id": "call_x",
                    "function": { "name": "f", "arguments": "{}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );

    let start_idx = evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .expect("clamped tool-call still opens a block");
    assert_eq!(start_idx, 0);

    let delta_idx = evs.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            index,
            delta: IrDelta::InputJsonDelta(_),
        } => Some(*index),
        _ => None,
    });
    assert_eq!(delta_idx, Some(start_idx));
}

#[test]
fn stream_tool_call_index_close_does_not_overflow_on_finish() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-c",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": u64::MAX,
                    "id": "call_y",
                    "function": { "name": "g", "arguments": "{}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-c",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_TOOL_CALLS}]
        }),
        &mut st,
    );
    let stop_idx = evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStop { index } => Some(*index),
            _ => None,
        })
        .expect("open tool block is closed on finish");
    assert_eq!(stop_idx, 0);
}

/// When a tool opens BEFORE any text, then text arrives, a later tool-argument delta must be
#[test]
fn stream_tool_arg_delta_uses_recorded_block_start_index() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();

    let open_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-c3", "created": 1_700_000_000u64, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_a",
                "function": {"name": "f", "arguments": ""}
            }]}, "finish_reason": null}]
        }),
        &mut st,
    );
    let tool_start_idx = open_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: crate::codec::ir::IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("tool BlockStart");

    let _ = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-c3", "created": 1_700_000_000u64, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]
        }),
        &mut st,
    );

    let arg_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-c3", "created": 1_700_000_000u64, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0,
                "function": {"arguments": "{\"x\":1}"}
            }]}, "finish_reason": null}]
        }),
        &mut st,
    );
    let arg_idx = arg_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::InputJsonDelta(_),
            } => Some(*index),
            _ => None,
        })
        .expect("tool arg InputJsonDelta");
    assert_eq!(
        arg_idx, tool_start_idx,
        "arg delta must land at the recorded BlockStart index ({tool_start_idx}), not a \
             recomputed index ({arg_idx})"
    );
}

#[test]
fn stream_tool_only_yields_zero_based_tool_indices() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let start_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-to",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": "call_a",
                    "function": { "name": "f", "arguments": "{\"x\":1}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let start_idx = start_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("tool-only stream opens a tool block");
    assert_eq!(
        start_idx, 0,
        "tool-only stream must be 0-based, not 1-based"
    );

    let delta_idx = start_evs.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            index,
            delta: IrDelta::InputJsonDelta(_),
        } => Some(*index),
        _ => None,
    });
    assert_eq!(delta_idx, Some(0));

    let finish_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-to",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_TOOL_CALLS}]
        }),
        &mut st,
    );
    let stop_idx = finish_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStop { index } => Some(*index),
            _ => None,
        })
        .expect("tool block is closed on finish");
    assert_eq!(
        stop_idx, 0,
        "tool BlockStop must pair with its 0-based start"
    );
}

#[test]
fn stream_text_then_tool_keeps_text_at_zero_tool_after() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();

    let text_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tt",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "content": "hello" },
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let text_idx = text_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Text,
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("text block opens");
    assert_eq!(text_idx, 0, "text owns index 0");

    let tool_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tt",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": "call_b",
                    "function": { "name": "g", "arguments": "{}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let tool_idx = tool_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("tool block opens after text");
    assert_eq!(tool_idx, 1, "tool follows the text block at index 1");

    let tool_stops: Vec<usize> = tool_evs
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStop { index } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_stops,
        vec![0],
        "the text block (0) must close on the tool chunk, before the tool opens: {tool_stops:?}"
    );

    let finish_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tt",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_TOOL_CALLS}]
        }),
        &mut st,
    );
    let stops: Vec<usize> = finish_evs
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStop { index } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops,
        vec![1],
        "finish closes only the still-open tool at 1: {stops:?}"
    );
}

#[test]
fn stream_tool_then_text_no_index_collision_and_stops_pair() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();

    let tool_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tf",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": "call_a",
                    "function": { "name": "f", "arguments": "{}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let tool_idx = tool_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("tool block opens first");
    assert_eq!(tool_idx, 0, "tool-first claims index 0");

    let text_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tf",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "content": "hello" },
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let text_idx = text_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Text,
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("text block opens after tool");
    assert_eq!(
        text_idx, 1,
        "text lands past the open tool, not colliding at 0"
    );
    assert_ne!(text_idx, tool_idx, "text and tool must not share an index");

    let tool2_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tf",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 1,
                    "id": "call_b",
                    "function": { "name": "g", "arguments": "{}" }
                }]},
                "finish_reason": null
            }]
        }),
        &mut st,
    );
    let tool2_idx = tool2_evs
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::ToolUse { .. },
                refusal: _,
            } => Some(*index),
            _ => None,
        })
        .expect("second tool opens");
    assert_eq!(tool2_idx, 2, "second tool lands past text");

    assert!(
        tool2_evs
            .iter()
            .any(|e| matches!(e, IrStreamEvent::BlockStop { index } if *index == 1)),
        "the text block (1) must close on the second tool chunk: {tool2_evs:?}"
    );

    let finish_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-tf",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_TOOL_CALLS}]
        }),
        &mut st,
    );

    let mut stops: Vec<usize> = tool2_evs
        .iter()
        .chain(finish_evs.iter())
        .filter_map(|e| match e {
            IrStreamEvent::BlockStop { index } => Some(*index),
            _ => None,
        })
        .collect();
    stops.sort_unstable();
    assert_eq!(
        stops,
        vec![0, 1, 2],
        "every BlockStart pairs with a BlockStop at the same index: {stops:?}"
    );
}

#[test]
fn stream_open_tools_is_capped() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let mut block_starts = 0usize;
    for i in 0..(MAX_OPEN_TOOLS as u64 + 50) {
        let evs = reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &serde_json::json!({
                "id": "chatcmpl-cap",
                "created": 1_700_000_000u64,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [{
                        // Distinct indices, all within the clamp ceiling so the cap (not the
                        // clamp) is what limits growth here.
                        "index": i.min(MAX_TOOL_INDEX),
                        "id": format!("call_{i}"),
                        "function": { "name": "f", "arguments": "{}" }
                    }]},
                    "finish_reason": null
                }]
            }),
            &mut st,
        );
        block_starts += evs
            .iter()
            .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
            .count();
    }

    assert!(st.open_tools.len() <= MAX_OPEN_TOOLS);

    assert!(block_starts <= MAX_OPEN_TOOLS);
}

#[test]
fn read_request_reads_array_form_tool_message_content() {
    let body = serde_json::json!({
        "model": "gpt-x",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_42",
                "content": [
                    { "type": "text", "text": "part one " },
                    { "type": "text", "text": "part two" }
                ]
            }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    let tool_msg = ir
        .messages
        .iter()
        .find(|m| m.role == IrRole::Tool)
        .expect("tool message present");
    let result = tool_msg
        .content
        .iter()
        .find_map(|b| match b {
            IrBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .expect("tool result block present");
    assert_eq!(result.0, "call_42");

    assert_eq!(result.1, vec![text_block("part one part two")]);
}

#[test]
fn read_request_reads_string_form_tool_message_content() {
    let body = serde_json::json!({
        "model": "gpt-x",
        "messages": [
            { "role": "tool", "tool_call_id": "call_7", "content": "plain string" }
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    let tool_msg = ir
        .messages
        .iter()
        .find(|m| m.role == IrRole::Tool)
        .expect("tool message present");
    let content = tool_msg
        .content
        .iter()
        .find_map(|b| match b {
            IrBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("tool result block present");
    assert_eq!(content, vec![text_block("plain string")]);
}

#[test]
fn write_error_emits_invalid_api_key_code_for_auth_failure() {
    let w = OpenAiWriter;
    let body = w.write_error(
        crate::codec::test_host::HOST,
        401,
        ERR_TYPE_AUTHENTICATION,
        "Incorrect API key provided",
    );
    assert_eq!(
        body["error"]["type"],
        serde_json::json!("authentication_error") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(body["error"]["code"], serde_json::json!("invalid_api_key"));
    assert_eq!(body["error"]["param"], serde_json::Value::Null);
}

#[test]
fn write_error_keeps_null_code_for_non_auth_errors() {
    let w = OpenAiWriter;
    for (status, kind) in [
        (400u16, ERR_TYPE_INVALID_REQUEST),
        (429, ERR_TYPE_RATE_LIMIT),
        (500, ERR_TYPE_SERVER_ERROR),
    ] {
        let body = w.write_error(crate::codec::test_host::HOST, status, kind, "boom");
        assert_eq!(
            body["error"]["code"],
            serde_json::Value::Null,
            "non-auth error must keep code: null (kind={kind})"
        );
    }
}

#[test]
fn stream_error_auth_event_carries_invalid_api_key_code() {
    let w = OpenAiWriter;
    let ev = IrStreamEvent::Error(IrError {
        class: busbar_contract::upstream::StatusClass::Auth,
        provider_signal: Some("bad key".to_string()),
        retry_after: None,
    });
    let (_, chunk) = w
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error event emits a body");
    assert_eq!(
        chunk["error"]["type"],
        serde_json::json!("authentication_error") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(chunk["error"]["code"], serde_json::json!("invalid_api_key"));
}

#[test]
fn stream_error_billing_event_maps_to_insufficient_quota() {
    let w = OpenAiWriter;
    let ev = IrStreamEvent::Error(IrError {
        class: busbar_contract::upstream::StatusClass::Billing,
        provider_signal: Some("over quota".to_string()),
        retry_after: None,
    });
    let (_, chunk) = w
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error event emits a body");

    assert_eq!(
        chunk["error"]["type"],
        serde_json::json!("insufficient_quota") // golden wire-contract literal (kept bare on purpose)
    );
    assert_ne!(
        chunk["error"]["type"],
        serde_json::json!("permission_error") // golden wire-contract literal (kept bare on purpose)
    );

    assert_eq!(
        chunk["error"]["code"],
        serde_json::json!("insufficient_quota") // golden wire-contract literal (kept bare on purpose)
    );

    let non_stream = w.write_error(
        crate::codec::test_host::HOST,
        429,
        ERR_TYPE_INSUFFICIENT_QUOTA,
        "over quota",
    );
    assert_eq!(
        chunk["error"]["type"], non_stream["error"]["type"],
        "stream and non-stream billing type must agree"
    );
    assert_eq!(
        chunk["error"]["code"], non_stream["error"]["code"],
        "stream and non-stream billing code must agree"
    );
}

#[test]
fn stream_message_delta_emits_usage_when_counts_nonzero() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 12,
            output_tokens: 34,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message delta emits a chunk");

    assert_eq!(
        chunk["choices"][0]["finish_reason"],
        serde_json::json!("stop")
    );

    assert_eq!(chunk["usage"]["prompt_tokens"], serde_json::json!(12));
    assert_eq!(chunk["usage"]["completion_tokens"], serde_json::json!(34));
    assert_eq!(chunk["usage"]["total_tokens"], serde_json::json!(46));
}

#[test]
fn stream_message_delta_omits_usage_when_all_counts_zero() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: IrUsage::default(),
        stop_detail: None,
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message delta emits a chunk");
    assert!(
        chunk.get("usage").is_none(),
        "zero usage must not emit a usage object: {chunk}"
    );
}

fn req_with_tool(
    input_schema: serde_json::Value,
    description: Option<&str>,
) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        tools: vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: description.map(String::from),
            input_schema,
            cache_control: None,
            hosted: None,
            strict: None,
        }],
        ..Default::default()
    }
}

#[test]
fn write_request_tools_use_nested_function_shape() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"city": {"type": "string"}}
    });
    let req = req_with_tool(schema.clone(), Some("Look up the weather"));
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let tool = &out["tools"][0];

    assert_eq!(tool["type"], serde_json::json!("function"));
    assert_eq!(tool["function"]["name"], serde_json::json!("get_weather"));
    assert_eq!(
        tool["function"]["description"],
        serde_json::json!("Look up the weather")
    );
    assert_eq!(tool["function"]["parameters"], schema);

    assert!(tool.get("name").is_none(), "name must not be flat");
    assert!(
        tool.get("parameters").is_none(),
        "parameters must not be flat"
    );
    assert!(
        tool.get("description").is_none(),
        "description must not be flat"
    );
}

#[test]
fn write_request_tool_round_trips_through_read_openai_tool() {
    let schema = serde_json::json!({"type": "object"});
    let req = req_with_tool(schema.clone(), Some("desc"));
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let ir = read_openai_tool(&out["tools"][0]).expect("nested tool parses");
    assert_eq!(ir.name, "get_weather");
    assert_eq!(ir.description.as_deref(), Some("desc"));
    assert_eq!(ir.input_schema, schema);
}

#[test]
fn write_request_tool_without_description_omits_it_inside_function() {
    let req = req_with_tool(serde_json::json!({"type": "object"}), None);
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let func = &out["tools"][0]["function"];
    assert!(func.get("description").is_none());

    assert!(func.get("parameters").is_some());
}

#[test]
fn write_request_tool_null_schema_defaults_to_empty_object_in_function() {
    let req = req_with_tool(serde_json::Value::Null, None);
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out["tools"][0]["function"]["parameters"],
        serde_json::json!({})
    );
}

#[test]
fn write_error_overloaded_maps_to_server_error() {
    for kind in [
        "overloaded",
        ERR_TYPE_OVERLOADED,
        "service_unavailable",
        "unavailable",
        "transient",
        "timeout",
        "network",
        "5xx",
    ] {
        let v = openai_writer().write_error(
            crate::codec::test_host::HOST,
            503,
            kind,
            "Service overloaded",
        );
        assert_eq!(
            v["error"]["type"],
            serde_json::json!("server_error"), // golden wire-contract literal (kept bare on purpose)
            "kind={kind} must map to server_error"
        );

        assert_ne!(v["error"]["type"], serde_json::json!("overloaded"));
        assert_eq!(v["error"]["code"], serde_json::Value::Null);
    }
}

#[test]
fn write_error_insufficient_quota_keeps_type_and_sets_code() {
    let v = openai_writer().write_error(
        crate::codec::test_host::HOST,
        429,
        ERR_TYPE_INSUFFICIENT_QUOTA,
        "quota exceeded",
    );
    assert_eq!(v["error"]["type"], serde_json::json!("insufficient_quota"));
    assert_eq!(v["error"]["code"], serde_json::json!("insufficient_quota"));
}

#[test]
fn read_openai_block_refusal_maps_to_text() {
    let block = serde_json::json!({"type": "refusal", "refusal": "I cannot help with that."});
    let ir = read_openai_block(&block).expect("refusal must not error");
    match ir {
        crate::codec::ir::IrBlock::Text { text, .. } => {
            assert_eq!(text, "I cannot help with that.")
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn read_openai_part_unknown_type_is_dropped_not_substituted() {
    let block = serde_json::json!({"type": "some_future_part", "foo": "bar"});
    let ir = read_openai_part(&block).expect("unknown type must not error");
    assert!(ir.is_none(), "an unknown part is dropped, got {ir:?}");
}

fn response_with_finish(finish: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-x",
        "object": OBJ_COMPLETION,
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": finish
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

#[test]
fn read_response_normalizes_content_filter_to_safety() {
    let ir = OpenAiReader
        .read_response(
            crate::codec::test_host::HOST,
            &response_with_finish(FINISH_CONTENT_FILTER),
        )
        .expect("parses");
    assert_eq!(ir.stop_reason, Some(crate::codec::ir::IrStopReason::Safety));
}

#[test]
fn read_response_normalizes_function_call_to_tool_use() {
    let ir = OpenAiReader
        .read_response(
            crate::codec::test_host::HOST,
            &response_with_finish(FINISH_FUNCTION_CALL),
        )
        .expect("parses");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
}

#[test]
fn write_response_safety_round_trips_to_content_filter() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::Safety),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("chatcmpl-x".to_string()),
        created: Some(1),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out["choices"][0]["finish_reason"],
        serde_json::json!("content_filter")
    );
}

#[test]
fn stream_message_delta_safety_round_trips_to_content_filter() {
    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::Safety),
        stop_sequence: None,
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (_, chunk) = openai_writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("message delta emits a chunk");
    assert_eq!(
        chunk["choices"][0]["finish_reason"],
        serde_json::json!("content_filter")
    );
}

#[test]
fn stream_read_normalizes_content_filter_to_safety() {
    let chunk = serde_json::json!({
        "id": "chatcmpl-x",
        "object": OBJ_CHUNK,
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_CONTENT_FILTER}]
    });
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state);
    let stop = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
        _ => None,
    });
    assert_eq!(stop, Some(crate::codec::ir::IrStopReason::Safety));
}

#[test]
fn singular_read_response_event_delegates_to_fanout() {
    let chunk = serde_json::json!({
        "id": "chatcmpl-x",
        "object": OBJ_CHUNK,
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
    });
    let ev = OpenAiReader.read_response_event(crate::codec::test_host::HOST, "", &chunk);
    assert!(
        matches!(ev, Some(IrStreamEvent::MessageStart { .. })),
        "singular event must surface the fan-out's first event, got {ev:?}"
    );
}

#[test]
fn singular_read_response_event_empty_chunk_yields_none() {
    let done = serde_json::Value::String(crate::codec::dialect::SSE_DONE_SENTINEL.to_string());
    assert!(OpenAiReader
        .read_response_event(crate::codec::test_host::HOST, "", &done)
        .is_none());
}

#[test]
fn null_usage_on_content_chunk_emits_no_message_delta() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": null}],
        "usage": null
    });
    let evs =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, IrStreamEvent::MessageDelta { .. })),
        "usage:null content chunk must not emit a MessageDelta, got {evs:?}"
    );
    assert!(
            evs.iter().any(|e| matches!(
                e,
                IrStreamEvent::BlockDelta { delta: crate::codec::ir::IrDelta::TextDelta(t), .. } if t == "hello"
            )),
            "text content must still decode, got {evs:?}"
        );
}

#[test]
fn trailing_usage_only_chunk_emits_message_delta_with_real_tokens() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let mut all = Vec::new();

    for chunk in [
        serde_json::json!({"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null}),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":FINISH_STOP}],"usage":null}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
    ] {
        all.extend(OpenAiReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut state,
        ));
    }

    let trailing = all.iter().rev().find_map(|e| match e {
        IrStreamEvent::MessageDelta {
            stop_reason: None,
            usage,
            ..
        } => Some(usage.clone()),
        _ => None,
    });
    let usage =
        trailing.expect("trailing usage-only chunk must emit a stop_reason:None MessageDelta");
    assert_eq!(
        usage.input_tokens, 7,
        "real prompt tokens must survive, got {usage:?}"
    );
    assert_eq!(
        usage.output_tokens, 3,
        "real completion tokens must survive, got {usage:?}"
    );

    assert_eq!(
        all.iter()
            .filter(|e| matches!(e, IrStreamEvent::MessageStop))
            .count(),
        1
    );
}

#[test]
fn read_response_tolerates_missing_usage() {
    let body = serde_json::json!({
        "id": "chatcmpl-x",
        "object": OBJ_COMPLETION,
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": FINISH_STOP
        }]
        // NOTE: no "usage" field.
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("a 200 body with no usage must read back, not hard-fail");
    assert_eq!(ir.usage.input_tokens, 0);
    assert_eq!(ir.usage.output_tokens, 0);
    assert_eq!(ir.usage.cache_read_input_tokens, None);

    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
    assert_eq!(ir.model.as_deref(), Some("gpt-4o"));
}

#[test]
fn write_request_string_tool_arguments_emitted_verbatim() {
    let raw = "not-json {oops".to_string();
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![IrMessage {
            role: IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "call_1".to_string(),
                name: "do_it".to_string(),
                input: serde_json::Value::String(raw.clone()),
                cache_control: None,
            }],
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let args = &out["messages"][0]["tool_calls"][0]["function"]["arguments"];
    assert_eq!(
        args,
        &serde_json::Value::String(raw),
        "string tool arguments must be emitted verbatim, not double-encoded, got {args}"
    );
}

#[test]
fn write_response_string_tool_arguments_emitted_verbatim() {
    let raw = "not-json {oops".to_string();
    let resp = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            thought_signature: None,
            id: "call_1".to_string(),
            name: "do_it".to_string(),
            input: serde_json::Value::String(raw.clone()),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("chatcmpl-x".to_string()),
        created: Some(1),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let args = &out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"];
    assert_eq!(
        args,
        &serde_json::Value::String(raw),
        "string tool arguments must be emitted verbatim, not double-encoded, got {args}"
    );
}

#[test]
fn late_reasoning_delta_after_text_does_not_shift_indices() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let c1 = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]
    });
    let evs1 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c1, &mut state);
    assert!(
        evs1.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: _,
            }
        )),
        "text block must open at index 0, got {evs1:?}"
    );
    assert!(state.text_block_open);
    assert!(!state.reasoning_seen);

    let c2 = serde_json::json!({
        "choices": [{"index": 0, "delta": {"reasoning_content": "late thought"}, "finish_reason": null}]
    });
    let evs2 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c2, &mut state);
    assert!(
        matches!(evs2.first(), Some(IrStreamEvent::BlockStop { index: 0 })),
        "the open text block closes at its own index first, got {evs2:?}"
    );
    assert!(
        evs2.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 1,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        )),
        "late reasoning opens a thinking block at the next free index, got {evs2:?}"
    );
    assert!(
        evs2.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta {
                index: 1,
                delta: crate::codec::ir::IrDelta::ThinkingDelta(_),
            }
        )),
        "late reasoning is carried as a ThinkingDelta, got {evs2:?}"
    );
    assert!(
        !state.reasoning_seen,
        "late reasoning must NOT flip reasoning_seen (which would shift already-opened indices)"
    );

    let c3 = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": null}]
    });
    let evs3 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c3, &mut state);
    assert!(
        matches!(evs3.first(), Some(IrStreamEvent::BlockStop { index: 1 })),
        "the thinking block closes at its own index, got {evs3:?}"
    );
    let text_idx = evs3.iter().find_map(|e| match e {
        IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::TextDelta(_),
        } => Some(*index),
        _ => None,
    });
    assert_eq!(
        text_idx,
        Some(2),
        "resumed text opens a fresh block past the thinking block, got {evs3:?}"
    );
}

#[test]
fn early_reasoning_delta_still_opens_thinking_at_index_0() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let c = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"reasoning_content": "thinking..."}, "finish_reason": null}]
    });
    let evs = OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c, &mut state);
    assert!(
        evs.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        )),
        "early reasoning must open a thinking block at index 0, got {evs:?}"
    );
    assert!(state.reasoning_seen);
}

#[test]
fn logprobs_only_chunk_closes_thinking_before_opening_text() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let c1 = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"reasoning_content": "thinking..."}, "finish_reason": null}]
    });
    let _ = OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c1, &mut state);
    assert!(state.thinking_block_open, "thinking block should be open");

    let c2 = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "delta": {},
            "logprobs": {"content": [{"token": "Hi", "logprob": -0.1, "bytes": [72, 105]}]},
            "finish_reason": null
        }]
    });
    let evs = OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c2, &mut state);

    let stop_pos = evs
        .iter()
        .position(|e| matches!(e, IrStreamEvent::BlockStop { index: 0 }));
    let start_pos = evs.iter().position(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::Text,
                ..
            }
        )
    });
    let stop_pos = stop_pos.unwrap_or_else(|| {
        panic!("thinking block must close with BlockStop{{index:0}}, got {evs:?}")
    });
    let start_pos = start_pos
        .unwrap_or_else(|| panic!("logprobs-only chunk must open a text block, got {evs:?}"));
    assert!(
        stop_pos < start_pos,
        "thinking BlockStop must precede text BlockStart (no two blocks open at once), got {evs:?}"
    );
    assert!(!state.thinking_block_open, "thinking block must be closed");
    assert!(state.text_block_open, "text block must be open");
}

#[test]
fn max_tokens_above_u32_max_is_rejected_not_truncated() {
    let reader = OpenAiReader;

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 4_294_967_296u64
    });
    let ir = reader.read_request(&body).expect("request parses");
    assert_eq!(
        ir.max_tokens, None,
        "max_tokens above u32::MAX must be rejected (None), not truncated to {:?}",
        ir.max_tokens
    );

    let body2 = serde_json::json!({
        "model": "o3",
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 4_294_967_296u64
    });
    let ir2 = reader.read_request(&body2).expect("request parses");
    assert_eq!(
        ir2.max_tokens, None,
        "max_completion_tokens above u32::MAX must be rejected, not truncated"
    );

    let body3 = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024u64
    });
    let ir3 = reader.read_request(&body3).expect("request parses");
    assert_eq!(ir3.max_tokens, Some(1024));
}

#[test]
fn auth_headers_valid_key_emits_bearer_authorization() {
    let Some(busbar_contract::protocol::EgressScheme::Static {
        families,
        own,
        passthrough,
    }) = crate::codec::openai_chat::DECL.egress_scheme
    else {
        panic!("openai declares a static credential scheme");
    };
    assert!(families.is_empty());
    assert_eq!(own, busbar_contract::protocol::CredentialHeader::Bearer);
    assert_eq!(
        passthrough,
        busbar_contract::protocol::CredentialHeader::Bearer
    );
    assert!(
        crate::codec::openai_chat::DECL
            .egress_auth_headers
            .is_none(),
        "no credential passes through the plane"
    );
}

#[test]
fn test_openai_tool_choice_required_roundtrips() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": "required",
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );

    assert!(!ir.extra.contains_key("tool_choice"));
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["tool_choice"], serde_json::json!("required"));
}

#[test]
fn test_openai_tool_choice_specific_function() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": {"type": TOOL_TYPE_FUNCTION, "function": {"name": "get_weather"}},
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string()
        })
    );
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["tool_choice"],
        serde_json::json!({"type": "function", "function": {"name": "get_weather"}}) // golden wire-contract literal (kept bare on purpose)
    );
}

#[test]
fn test_openai_tool_choice_absent_is_none() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.tool_choice, None);
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("tool_choice").is_none(),
        "no tool_choice should be emitted when the caller omitted it"
    );
}

/// Minimal valid `IrRequest` for writer-side tool_choice/temperature tests.
fn test_ir_request() -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![text_block("hi")],
        }],
        ..Default::default()
    }
}

/// Opt-out strips usage: with `client_include_usage == false` (the default), the OpenAI
#[test]
fn test_framing_gates_usage_on_client_include_usage() {
    use super::super::proto_codec::StreamFraming;

    fn folded_finish() -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-abc",
            "object": OBJ_CHUNK,
            "created": 1234,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {}, "finish_reason": FINISH_STOP}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
        })
    }

    let mut off = OpenAiStreamFraming::default();
    let mut finish = folded_finish();
    let trailing = off.on_egress_chunk(&mut finish);
    assert!(
        trailing.is_none(),
        "an opted-out client must get NO trailing usage chunk: {trailing:?}"
    );
    assert!(
        finish.get("usage").is_none(),
        "the folded usage must be stripped off the finish chunk: {finish}"
    );
    assert_eq!(
        finish.pointer("/choices/0/finish_reason"),
        Some(&serde_json::json!(FINISH_STOP)),
        "the finish chunk keeps its finish_reason"
    );

    let mut on = OpenAiStreamFraming::default();
    on.set_client_include_usage(true);
    let mut finish2 = folded_finish();
    let trailing2 = on
        .on_egress_chunk(&mut finish2)
        .expect("opt-in must un-fold a trailing usage chunk");
    assert!(
        finish2.get("usage").is_none(),
        "usage is lifted OFF the finish chunk on opt-in too: {finish2}"
    );
    assert_eq!(
        trailing2.pointer("/usage/total_tokens"),
        Some(&serde_json::json!(10)),
        "the trailing chunk carries the usage: {trailing2}"
    );
    assert_eq!(
        trailing2.pointer("/choices"),
        Some(&serde_json::json!([])),
        "the trailing usage chunk carries an EMPTY choices array"
    );
}

/// A frame's payload bytes, as the same-proto edit seam reads them.
fn bytes(payload: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(payload).expect("serializes")
}

/// Usage-strip object gate: an OpenAI-COMPATIBLE upstream may omit the `object` field on its
#[test]
fn strip_same_proto_usage_fires_without_object_field() {
    use super::super::proto_codec::StreamFraming;

    let framing = OpenAiStreamFraming::default();

    let no_object_content = serde_json::json!({
        "id": "chatcmpl-x",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": serde_json::Value::Null}],
        "usage": serde_json::Value::Null
    });
    assert!(
        framing.same_proto_edit(&bytes(&no_object_content)) == SameProtoEdit::StripUsage,
        "an opted-out client must have the usage:null tell stripped even when the chunk omits `object`"
    );

    let variant_object = serde_json::json!({
        "object": "chat.completion.chunk.delta",
        "choices": [{"index": 0, "delta": {"content": "yo"}}],
        "usage": serde_json::Value::Null
    });
    assert!(
        framing.same_proto_edit(&bytes(&variant_object)) == SameProtoEdit::StripUsage,
        "a compatible upstream using an `object` variant must still have the usage tell stripped"
    );

    let usage_only_trailer = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    });
    assert!(
        framing.same_proto_edit(&bytes(&usage_only_trailer)) != SameProtoEdit::StripUsage,
        "the empty-choices usage-only trailer must not be strip-matched here"
    );

    let mut opted_in = OpenAiStreamFraming::default();
    opted_in.set_client_include_usage(true);
    assert!(
        opted_in.same_proto_edit(&bytes(&no_object_content)) == SameProtoEdit::Keep,
        "an opted-in client's requested usage must never be stripped"
    );
}

/// Usage-trailer suppression object gate: an OpenAI-COMPATIBLE upstream may omit (or vary) the
#[test]
fn suppress_same_proto_frame_fires_without_object_field() {
    use super::super::proto_codec::StreamFraming;

    let framing = OpenAiStreamFraming::default();

    let no_object_trailer = serde_json::json!({
        "id": "chatcmpl-x",
        "choices": [],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    });
    assert!(
        framing.same_proto_edit(&bytes(&no_object_trailer)) == SameProtoEdit::Drop,
        "an opted-out client must not receive the usage-only trailer even when it omits `object`"
    );

    let variant_object = serde_json::json!({
        "object": "chat.completion.chunk.delta",
        "choices": [],
        "usage": {"total_tokens": 10}
    });
    assert!(
        framing.same_proto_edit(&bytes(&variant_object)) == SameProtoEdit::Drop,
        "a compatible upstream using an `object` variant must still have its trailer suppressed"
    );

    let content = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": "hi"}}],
        "usage": {"total_tokens": 10}
    });
    assert!(
        framing.same_proto_edit(&bytes(&content)) != SameProtoEdit::Drop,
        "a content chunk carries the client's tokens and must never be dropped"
    );

    let mut opted_in = OpenAiStreamFraming::default();
    opted_in.set_client_include_usage(true);
    assert!(
        opted_in.same_proto_edit(&bytes(&no_object_trailer)) == SameProtoEdit::Keep,
        "an opted-in client's requested usage trailer must never be suppressed"
    );
}

/// 0-based streaming tool_calls index (unit): `remap_tool_call_index` rewrites the
#[test]
fn test_remap_tool_call_index_is_0_based_per_call() {
    use super::super::proto_codec::StreamFraming;
    let mut framing = OpenAiStreamFraming::default();

    let mut open1 = serde_json::json!({
        "object": OBJ_CHUNK,
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 1, "id": "call_a", "type": "function",
            "function": {"name": "get_weather", "arguments": ""}
        }]}, "finish_reason": null}]
    });
    framing.on_egress_chunk(&mut open1);
    assert_eq!(
        open1.pointer("/choices/0/delta/tool_calls/0/index"),
        Some(&serde_json::json!(0)),
        "first tool call (raw index 1) must remap to 0: {open1}"
    );

    let mut arg1 = serde_json::json!({
        "object": OBJ_CHUNK,
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 1, "function": {"arguments": "{}"}
        }]}, "finish_reason": null}]
    });
    framing.on_egress_chunk(&mut arg1);
    assert_eq!(
        arg1.pointer("/choices/0/delta/tool_calls/0/index"),
        Some(&serde_json::json!(0)),
        "argument fragment for the first call must replay ordinal 0: {arg1}"
    );

    let mut open2 = serde_json::json!({
        "object": OBJ_CHUNK,
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 2, "id": "call_b", "type": "function",
            "function": {"name": "get_time", "arguments": ""}
        }]}, "finish_reason": null}]
    });
    framing.on_egress_chunk(&mut open2);
    assert_eq!(
        open2.pointer("/choices/0/delta/tool_calls/0/index"),
        Some(&serde_json::json!(1)),
        "second tool call (raw index 2) must remap to 1: {open2}"
    );

    let mut content = serde_json::json!({
        "object": OBJ_CHUNK,
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]
    });
    let before = content.clone();
    framing.on_egress_chunk(&mut content);
    assert_eq!(
        content, before,
        "a content chunk must be untouched by the tool-index remap"
    );
}

/// N>1 cross-protocol: a request asking for N>1 candidate completions cannot round-trip
#[test]
fn test_n_gt_1_clamped_to_one_on_cross_protocol_egress() {
    use crate::codec::egress_prep::EgressPrep;

    fn prep() -> EgressPrep {
        EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: "openai",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: true,
            reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        }
    }

    let mut ir = test_ir_request();
    ir.n = Some(3);
    let mut req = ir;
    super::super::chat_handle::chat_prepare_for_egress(&mut req, &prep());
    let ir = req;
    assert_eq!(
        ir.n,
        Some(1),
        "n>1 must clamp to 1 on the cross-protocol seam"
    );
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["n"],
        serde_json::json!(1),
        "the egress request must ask the backend for exactly one candidate"
    );

    let mut ir1 = test_ir_request();
    ir1.n = Some(1);
    let mut req1 = ir1;
    super::super::chat_handle::chat_prepare_for_egress(&mut req1, &prep());
    let ir1 = req1;
    assert_eq!(ir1.n, Some(1), "n=1 is unchanged");

    let mut ir0 = test_ir_request();
    ir0.n = None;
    let mut req0 = ir0;
    super::super::chat_handle::chat_prepare_for_egress(&mut req0, &prep());
    let ir0 = req0;
    assert_eq!(ir0.n, None, "absent n stays absent");
}

#[test]
fn test_openai_tool_choice_auto_roundtrips() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": "auto",
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.tool_choice, Some(crate::codec::ir::IrToolChoice::Auto));
    assert!(!ir.extra.contains_key("tool_choice"));
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["tool_choice"], serde_json::json!("auto"));
}

#[test]
fn test_openai_tool_choice_none_roundtrips() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        "tool_choice": "none",
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.tool_choice, Some(crate::codec::ir::IrToolChoice::None));
    assert!(!ir.extra.contains_key("tool_choice"));
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["tool_choice"], serde_json::json!("none"));
}

#[test]
fn test_anthropic_to_openai_tool_choice_directions() {
    use crate::codec::ir::IrToolChoice;
    let cases = [
        (IrToolChoice::Auto, serde_json::json!("auto")),
        (IrToolChoice::None, serde_json::json!("none")),
        (IrToolChoice::Required, serde_json::json!("required")),
        (
            IrToolChoice::Tool {
                name: "get_weather".to_string(),
            },
            serde_json::json!({"type": "function", "function": {"name": "get_weather"}}),
        ),
    ];
    for (tc, expected) in cases {
        let ir = crate::codec::ir::IrRequest {
            tool_choice: Some(tc.clone()),
            tools: vec![crate::codec::ir::IrTool {
                name: "get_weather".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
                cache_control: None,
                hosted: None,
                strict: None,
            }],
            ..test_ir_request()
        };
        let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["tool_choice"], expected, "tool_choice {tc:?}");
    }
}

#[test]
fn test_openai_tool_choice_unknown_string_is_none() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "definitely_not_a_real_mode",
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(
        ir.tool_choice, None,
        "an unrecognized tool_choice string must degrade to None, never Auto/Required"
    );
}

#[test]
fn test_openai_tool_choice_unknown_object_is_none() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "something_else"},
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.tool_choice, None);
}

#[test]
fn phase0_sampling_fields_read_into_ir() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "frequency_penalty": 0.5,
        "presence_penalty": -0.25,
        "seed": 42,
        "n": 3,
        "response_format": { "type": RESP_FORMAT_JSON_OBJECT }
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert_eq!(ir.frequency_penalty, Some(0.5_f64));
    assert_eq!(ir.presence_penalty, Some(-0.25_f64));
    assert_eq!(ir.seed, Some(42_i64));
    assert_eq!(ir.n, Some(3_u32));
    assert_eq!(
        ir.response_format,
        Some(crate::codec::ir::IrResponseFormat {
            json: true,
            schema: None,
            name: None,
            strict: None,
            description: None,
        })
    );

    assert!(!ir.extra.contains_key("frequency_penalty"));
    assert!(!ir.extra.contains_key("presence_penalty"));
    assert!(!ir.extra.contains_key("seed"));
    assert!(!ir.extra.contains_key("n"));
    assert!(!ir.extra.contains_key("response_format"));
}

#[test]
fn phase0_sampling_fields_written_from_ir() {
    let ir = crate::codec::ir::IrRequest {
        frequency_penalty: Some(1.5),
        presence_penalty: Some(-2.0),
        seed: Some(1234),
        n: Some(4),
        response_format: Some(crate::codec::ir::IrResponseFormat {
            json: true,
            schema: Some(serde_json::json!({"type": "object"})),
            name: Some("out".to_string()),
            strict: None,
            description: None,
        }),
        ..test_ir_request()
    };
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["frequency_penalty"], serde_json::json!(1.5));
    assert_eq!(out["presence_penalty"], serde_json::json!(-2.0));
    assert_eq!(out["seed"], serde_json::json!(1234));
    assert_eq!(out["n"], serde_json::json!(4));
    assert_eq!(
        out["response_format"],
        serde_json::json!({
            "type": "json_schema", // golden wire-contract literal (kept bare on purpose)
            "json_schema": {"name": "out", "schema": {"type": "object"}}
        }),
        "response_format must be re-emitted verbatim"
    );
}

#[test]
fn phase0_sampling_fields_omitted_when_absent() {
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &test_ir_request());
    let obj = out.as_object().expect("object");
    assert!(obj.get("frequency_penalty").is_none());
    assert!(obj.get("presence_penalty").is_none());
    assert!(obj.get("seed").is_none());
    assert!(obj.get("n").is_none());
    assert!(obj.get("response_format").is_none());
}

#[test]
fn phase0_sampling_fields_roundtrip_same_protocol() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "frequency_penalty": 0.75,
        "presence_penalty": 0.1,
        "seed": -7,
        "n": 2,
        "response_format": { "type": RESP_FORMAT_JSON_OBJECT }
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["frequency_penalty"], serde_json::json!(0.75));
    assert_eq!(out["presence_penalty"], serde_json::json!(0.1));
    assert_eq!(out["seed"], serde_json::json!(-7));
    assert_eq!(out["n"], serde_json::json!(2));
    assert_eq!(
        out["response_format"],
        serde_json::json!({ "type": "json_object" }) // golden wire-contract literal (kept bare on purpose)
    );
}

/// A Responses `file_id` image (the FILE_ID_IMAGE_SENTINEL media_type) reaching the OpenAI
#[test]
fn test_write_request_file_id_image_dropped_not_corrupted() {
    let writer = OpenAiWriter;
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: vec![],
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("describe this".to_string()),
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Vendor {
                        vendor: "responses",
                        value: serde_json::json!({ "file_id": "file-abc123" }),
                    },
                    cache_control: None,
                    detail: None,
                },
            ],
        }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains("file-abc123") && !wire.contains("file_id"),
        "a file_id image must not leak onto the OpenAI wire (no corrupt image_url); got {wire}"
    );

    let content = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .expect("user message content array");
    assert!(
        content
            .iter()
            .all(|p| p.get("type").and_then(|t| t.as_str()) != Some("image_url")),
        "no image_url part may be emitted for a file_id image; got {out}"
    );
    assert!(
        content
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("text")),
        "the text part must still survive; got {out}"
    );
}

/// HIGH (asymmetric twin of the file_id leak): a Bedrock S3-source image (the IMAGE_S3_SENTINEL
#[test]
fn test_write_request_image_s3_dropped_not_corrupted() {
    let writer = OpenAiWriter;
    let req = crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: vec![],
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("describe this".to_string()),
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Vendor {
                        vendor: "bedrock",
                        value: serde_json::json!({ "format": "png", "s3Location": { "uri": "s3://bucket/key.png" } }),
                    },
                    cache_control: None,
                    detail: None,
                },
            ],
        }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: vec![],
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let wire = serde_json::to_string(&out).unwrap();
    assert!(
        !wire.contains("image_s3")
            && !wire.contains("s3://bucket/key.png")
            && !wire.contains("s3Location"),
        "an image_s3 image must not leak onto the OpenAI wire (no corrupt image_url); got {wire}"
    );
    let content = out
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .expect("user message content array");
    assert!(
        content
            .iter()
            .all(|p| p.get("type").and_then(|t| t.as_str()) != Some("image_url")),
        "no image_url part may be emitted for an image_s3 image; got {out}"
    );
    assert!(
        content
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("text")),
        "the text part must still survive; got {out}"
    );
}

#[test]
fn test_modeled_request_keys_are_the_mapping_rows() {
    for k in [
        "model",
        "messages",
        "tools",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "stop",
        "stream",
        "tool_choice",
        "frequency_penalty",
        "presence_penalty",
        "seed",
        "n",
        "response_format",
        "user",
        "parallel_tool_calls",
        "logprobs",
        "top_logprobs",
        "reasoning_effort",
    ] {
        assert!(
            crate::codec::carry::models(super::map::REQUEST, k),
            "modeled key set must contain {k}"
        );
    }
    assert!(!crate::codec::carry::models(
        super::map::REQUEST,
        "unknown_passthrough_key"
    ));
}

/// Non-stream `read_response` cache-token NORMALIZATION (ir.rs:457): OpenAI's `prompt_tokens` is a
#[test]
fn read_response_subtracts_cached_prefix_from_prompt_tokens() {
    let body = serde_json::json!({
        "id": "chatcmpl-abc",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": FINISH_STOP
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "total_tokens": 105,
            "prompt_tokens_details": {"cached_tokens": 80}
        }
    });
    let resp = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.usage.input_tokens, 20,
        "input_tokens must be prompt_tokens(100) MINUS cached(80) = 20 (uncached)"
    );
    assert_eq!(resp.usage.cache_read_input_tokens, Some(80));
    assert_eq!(resp.usage.output_tokens, 5);
    assert_eq!(resp.usage.cache_creation_input_tokens, None);

    assert_eq!(resp.usage.billable_tokens(), 105);
}

/// The write side RECONSTRUCTS the native wire shape: it must ADD the cached prefix BACK onto the
#[test]
fn write_response_reconstructs_prompt_tokens_total_with_cached_details() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 20,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(80),
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("chatcmpl-abc".to_string()),
        created: Some(1_700_000_000),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out["usage"]["prompt_tokens"],
        serde_json::json!(100),
        "prompt_tokens must re-add the cached prefix: uncached(20) + cached(80) = 100"
    );
    assert_eq!(out["usage"]["completion_tokens"], serde_json::json!(5));
    assert_eq!(out["usage"]["total_tokens"], serde_json::json!(105));
    assert_eq!(
        out["usage"]["prompt_tokens_details"]["cached_tokens"],
        serde_json::json!(80)
    );
}

/// A cache-free response must NOT emit a spurious `prompt_tokens_details` object — matching the
#[test]
fn write_response_omits_cached_details_when_no_cache_read() {
    let mut resp = crate::codec::ir::IrResponse {
        content: vec![text_block("hi")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 7,
            output_tokens: 3,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("chatcmpl-x".to_string()),
        created: Some(1),
        ..Default::default()
    };
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(out["usage"]["prompt_tokens"], serde_json::json!(7));
    assert!(
        out["usage"].get("prompt_tokens_details").is_none(),
        "no cache read means no prompt_tokens_details object"
    );

    resp.usage.cache_read_input_tokens = Some(0);
    let out2 = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out2["usage"]["prompt_tokens_details"]["cached_tokens"],
        serde_json::json!(0)
    );
}

/// A `response_format` json_schema directive must round-trip read→IR→write byte-faithful: the
#[test]
fn response_format_json_schema_round_trips() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"]
    });
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "Person",
                "schema": schema,
                "strict": true,
                "description": "a person"
            }
        }
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    let rf = ir
        .response_format
        .as_ref()
        .expect("response_format must canonicalize into the typed IR");
    assert!(rf.json);
    assert_eq!(rf.name.as_deref(), Some("Person"));
    assert_eq!(rf.strict, Some(true));
    assert_eq!(rf.description.as_deref(), Some("a person"));
    assert_eq!(rf.schema.as_ref(), Some(&schema));

    assert!(!ir.extra.contains_key("response_format"));

    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["response_format"]["type"],
        serde_json::json!("json_schema")
    );
    assert_eq!(
        out["response_format"]["json_schema"]["name"],
        serde_json::json!("Person")
    );
    assert_eq!(out["response_format"]["json_schema"]["schema"], schema);
    assert_eq!(
        out["response_format"]["json_schema"]["strict"],
        serde_json::json!(true)
    );
}

/// An UNKNOWN native `finish_reason` (a token OpenAI may add after this build) reads to
#[test]
fn read_response_unknown_finish_reason_maps_to_other_and_degrades_to_stop() {
    assert_eq!(
        read_openai_stop_reason("some_future_reason"),
        crate::codec::ir::IrStopReason::Other
    );
    let body = serde_json::json!({
        "id": "chatcmpl-abc",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "some_future_reason"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let resp = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Other)
    );
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out["choices"][0]["finish_reason"],
        serde_json::json!("stop"),
        "Other must degrade to the safe stop default, never leak the foreign token"
    );
}

/// Multi-element `stop` must emit as a JSON ARRAY on the wire (not a bare string), and an empty
#[test]
fn write_request_stop_sequences_array_and_empty_omitted() {
    let mut req = test_ir_request();
    req.stop = vec![".".to_string(), "!".to_string()];
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        out["stop"],
        serde_json::json!([".", "!"]),
        "multi-element stop must serialize as an array"
    );

    let mut empty = test_ir_request();
    empty.stop = vec![];
    let out_empty = openai_writer().write_request(crate::codec::test_host::HOST, &empty);
    assert!(
        out_empty.get("stop").is_none(),
        "an empty stop vec must be omitted, not emitted as []"
    );
}

/// A structured-outputs / safety refusal arrives as `content: null` + `refusal: "..."` with
#[test]
fn read_response_surfaces_a_refusal_as_text_and_promotes_the_stop_reason() {
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "model": "gpt-4o-2024-08-06",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7}
    });

    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.content,
        vec![IrBlock::Text {
            text: "I can't help with that.".to_string(),
            cache_control: None,
            citations: Vec::new(),
            refusal: true,
        }],
        "the refusal text must not be dropped, and is flagged as the refusal (IR-02)"
    );
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::Refusal),
        "finish_reason `stop` on a refusal must be promoted to Refusal"
    );
}

/// A refusal that ALSO hit the length cap keeps MaxTokens — the promotion overrides EndTurn only.
#[test]
fn read_response_refusal_does_not_override_a_length_stop() {
    let body = serde_json::json!({
        "id": "chatcmpl-2",
        "model": "gpt-4o-2024-08-06",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "refusal": "I can't"},
            "finish_reason": "length"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7}
    });

    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.stop_reason,
        Some(crate::codec::ir::IrStopReason::MaxTokens)
    );
}

/// The streaming refusal is the same defect on the other path: `delta.refusal` carries the text in
#[test]
fn stream_refusal_deltas_open_a_text_block_and_promote_the_stop_reason() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let c1 = serde_json::json!({
        "choices": [{"index": 0, "delta": {"role": "assistant", "refusal": "I can't "}, "finish_reason": null}]
    });
    let evs1 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c1, &mut state);
    assert!(
        evs1.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Text,
                refusal: _,
            }
        )),
        "a refusal delta must open a text block, got {evs1:?}"
    );
    assert!(
        evs1.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::TextDelta(t)
            } if t == "I can't "
        )),
        "the refusal text must be emitted, got {evs1:?}"
    );
    assert!(state.refusal_seen);

    let c2 = serde_json::json!({
        "choices": [{"index": 0, "delta": {"refusal": "help with that."}, "finish_reason": null}]
    });
    let evs2 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c2, &mut state);
    assert!(
        evs2.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::TextDelta(t)
            } if t == "help with that."
        )),
        "a continuing refusal delta must land on the same block, got {evs2:?}"
    );

    let c3 = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let evs3 =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &c3, &mut state);
    assert!(
        evs3.iter().any(|e| matches!(
            e,
            IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::Refusal),
                ..
            }
        )),
        "the terminal frame must carry the promoted Refusal stop reason, got {evs3:?}"
    );
}

/// Chat Completions has a native `annotations`/`url_citation` shape, and the writer emitted no
#[test]
fn write_response_carries_citations_with_join_relative_offsets() {
    let cite = |quote: &str, title: &str, url: &str| crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".into()),
        cited_text: Some(quote.into()),
        title: Some(title.into()),
        url: Some(url.into()),
        document_index: None,
        start_index: None,
        end_index: None,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };
    let text_block = |text: &str, citations: Vec<crate::codec::ir::IrCitation>| IrBlock::Text {
        text: text.into(),
        cache_control: None,
        citations,
        refusal: false,
    };

    let resp = crate::codec::ir::IrResponse {
        id: Some("chatcmpl-c".into()),
        model: Some("gpt-4o".into()),
        created: Some(1_700_000_000),
        content: vec![
            text_block(
                "First claim. ",
                vec![cite("First claim.", "A", "https://a.test")],
            ),
            text_block(
                "Second claim.",
                vec![cite("Second claim.", "B", "https://b.test")],
            ),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };

    let v = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let anns = v["choices"][0]["message"]["annotations"]
        .as_array()
        .expect("annotations must be emitted when sources exist");
    assert_eq!(anns.len(), 2, "{anns:?}");

    assert_eq!(anns[0]["type"], "url_citation");
    assert_eq!(anns[0]["url_citation"]["url"], "https://a.test");
    assert_eq!(anns[0]["url_citation"]["start_index"], 0);
    assert_eq!(
        anns[1]["url_citation"]["start_index"], 13,
        "the second block's citation is offset by the first block's length in the joined content"
    );
    assert_eq!(anns[1]["url_citation"]["end_index"], 26);
}

/// `start_index: i64::MAX` (upstream-controlled, only sign-checked) must not panic on
#[test]
fn url_annotation_offset_saturates_on_absurd_upstream_index() {
    let citations = vec![crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".into()),
        cited_text: None,
        title: Some("T".into()),
        url: Some("https://example.com".into()),
        document_index: None,
        start_index: Some(i64::MAX),
        end_index: Some(i64::MAX),
        encrypted_index: None,
        raw: None,
        ..Default::default()
    }];

    let anns = super::super::url_citation_wire::url_annotations("hi", 5, &citations);
    assert_eq!(anns.len(), 1, "{anns:?}");
    assert_eq!(
        anns[0]["start_index"],
        serde_json::json!(i64::MAX),
        "saturating_add on i64::MAX + a small base stays i64::MAX, not wraps: {anns:?}"
    );
}

/// The writer's base accumulator must advance in CHARACTERS, not bytes — the
#[test]
fn url_annotation_base_accumulates_in_characters() {
    let cite = |quote: &str, url: &str| crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".into()),
        cited_text: Some(quote.into()),
        title: None,
        url: Some(url.into()),
        document_index: None,
        start_index: None,
        end_index: None,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };

    let first_block = "héllo wörld ";
    assert_eq!(first_block.chars().count(), 12);
    assert_eq!(first_block.len(), 14);

    let resp = crate::codec::ir::IrResponse {
        id: Some("chatcmpl-c".into()),
        model: Some("gpt-4o".into()),
        created: Some(1_700_000_000),
        content: vec![
            IrBlock::Text {
                text: first_block.to_string(),
                cache_control: None,
                citations: vec![],
                refusal: false,
            },
            IrBlock::Text {
                text: "claim.".to_string(),
                cache_control: None,
                citations: vec![cite("claim.", "https://b.test")],
                refusal: false,
            },
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let v = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let anns = v["choices"][0]["message"]["annotations"]
        .as_array()
        .expect("annotations must be emitted");
    assert_eq!(anns.len(), 1, "{anns:?}");
    assert_eq!(
        anns[0]["url_citation"]["start_index"], 12,
        "the second block's citation must be offset by the CHARACTER length (12) of the first \
         block, not its byte length (14): {anns:?}"
    );
}

/// The quote-recovery arm's `text.find(q)`/`q.len()` are byte offsets/lengths;
#[test]
fn url_annotation_quote_recovery_reports_character_indices() {
    let citations = vec![crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".into()),
        cited_text: Some("wörld".into()),
        title: None,
        url: Some("https://example.com".into()),
        document_index: None,
        start_index: None,
        end_index: None,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    }];

    let text = "héllo wörld";
    let anns = super::super::url_citation_wire::url_annotations(text, 0, &citations);
    assert_eq!(anns.len(), 1, "{anns:?}");
    assert_eq!(
        anns[0]["start_index"], 6,
        "the quote's start must be the CHARACTER offset (6), not the byte offset (7): {anns:?}"
    );
    assert_eq!(
        anns[0]["end_index"], 11,
        "the quote's end must be the CHARACTER offset (11), not the byte offset (13): {anns:?}"
    );
}

/// A completion with no sources must carry NO annotations key. OpenAI omits it when none were
#[test]
fn write_response_omits_annotations_when_there_are_no_citations() {
    let resp = crate::codec::ir::IrResponse {
        id: Some("chatcmpl-n".into()),
        model: Some("gpt-4o".into()),
        created: Some(1_700_000_000),
        content: vec![IrBlock::text("plain")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };
    let v = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    assert!(v["choices"][0]["message"].get("annotations").is_none());
}

/// `image_url.detail` is carried in the Image block's typed slot (IR-08) — `high` reaches the IR
#[test]
fn image_detail_is_carried_and_only_an_unknown_word_warns() {
    let body = |detail: &str| {
        serde_json::json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "https://example.com/x.png", "detail": detail}
                }]
            }]
        })
    };
    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let ir = tracing::dispatcher::with_default(&subscriber, || {
        OpenAiReader.read_request(&body("high")).expect("parses")
    });
    assert!(
        matches!(
            ir.messages[0].content[0],
            IrBlock::Image {
                detail: Some(crate::codec::ir::IrImageDetail::High),
                ..
            }
        ),
        "{:?}",
        ir.messages[0].content
    );
    assert!(
        !diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("detail")),
        "{:?}",
        diagnostic_texts(&cap)
    );
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["messages"][0]["content"][0]["image_url"]["detail"],
        serde_json::json!("high")
    );

    let (cap2, cap2_subscriber) = warnings();
    let subscriber2 = cap2_subscriber.clone();
    let _ir2 = tracing::dispatcher::with_default(&subscriber2, || {
        OpenAiReader
            .read_request(&body("original"))
            .expect("parses")
    });
    assert!(
        diagnostic_texts(&cap2)
            .iter()
            .any(|event| event.contains("detail")),
        "an unknown detail word must warn on drop: {:?}",
        diagnostic_texts(&cap2)
    );
}

/// REGRESSION PROOF: an Anthropic-sourced citation's
#[test]
fn anthropic_sourced_citation_indices_are_not_double_converted() {
    let citations = vec![crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".into()),
        cited_text: None,
        title: Some("T".into()),
        url: Some("https://example.com".into()),
        document_index: None,
        start_index: Some(6),
        end_index: Some(11),
        encrypted_index: None,
        raw: None,
        ..Default::default()
    }];
    let anns = super::super::url_citation_wire::url_annotations("héllo wörld", 0, &citations);
    assert_eq!(anns.len(), 1, "{anns:?}");
    assert_eq!(
        anns[0]["start_index"], 6,
        "an already-character offset must pass through UNCHANGED, not be converted again: {anns:?}"
    );
    assert_eq!(
        anns[0]["end_index"], 11,
        "an already-character offset must pass through UNCHANGED, not be converted again: {anns:?}"
    );
}

/// A Chat response's `message.annotations` (`url_citation` entries) must be read into the IR
#[test]
fn chat_response_annotations_read_into_ir_citations() {
    let body = serde_json::json!({
        "id": "chatcmpl-cite1",
        "object": OBJ_COMPLETION,
        "created": 1_700_000_000u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "See the source for details.",
                "annotations": [{
                    "type": "url_citation",
                    "url_citation": {
                        "url": "https://example.com/a",
                        "title": "Example A",
                        "start_index": 7,
                        "end_index": 20
                    }
                }]
            },
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let text_block = ir
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations),
            _ => None,
        })
        .expect("a Text block must be present");
    assert_eq!(text_block.len(), 1, "{text_block:?}");
    let c = &text_block[0];
    assert_eq!(c.url.as_deref(), Some("https://example.com/a"));
    assert_eq!(c.title.as_deref(), Some("Example A"));
    assert_eq!(
        c.kind.as_deref(),
        Some("web_search_result_location"),
        "routes downstream writers to their web-search citation shape"
    );

    assert_eq!(
        (c.start_index, c.end_index),
        (Some(7), Some(20)),
        "the url_citation span must carry: {c:?}"
    );
}

/// A `url_citation` annotation with no usable `url` must be skipped — symmetric with
#[test]
fn openai_annotation_without_a_url_is_skipped() {
    let body = serde_json::json!({
        "id": "chatcmpl-cite2",
        "object": OBJ_COMPLETION,
        "created": 1_700_000_000u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "No usable source.",
                "annotations": [{
                    "type": "url_citation",
                    "url_citation": {"title": "Untitled", "start_index": 0, "end_index": 5}
                }]
            },
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let text_block = ir
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations),
            _ => None,
        })
        .expect("a Text block must be present");
    assert!(
        text_block.is_empty(),
        "a url_citation with no url must be skipped, not fabricated: {text_block:?}"
    );
}

/// An OpenAI URL citation remains in the IR, but lacks the native source text and opaque index
#[test]
fn openai_chat_citation_unavailable_on_anthropic_is_audited() {
    let body = serde_json::json!({
        "id": "chatcmpl-cite3",
        "object": OBJ_COMPLETION,
        "created": 1_700_000_000u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "See the source for details.",
                "annotations": [{
                    "type": "url_citation",
                    "url_citation": {
                        "url": "https://example.com/hop",
                        "title": "Hop Source",
                        "start_index": 7,
                        "end_index": 20
                    }
                }]
            },
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let citation = ir
        .content
        .iter()
        .find_map(|block| match block {
            IrBlock::Text { citations, .. } => citations.first(),
            _ => None,
        })
        .expect("the source URL annotation is retained in the IR");
    assert_eq!(citation.url.as_deref(), Some("https://example.com/hop"));
    assert_eq!(citation.title.as_deref(), Some("Hop Source"));
    assert_eq!(
        (citation.start_index, citation.end_index),
        (Some(7), Some(20))
    );
    let (out, dropped) = crate::codec::drops::scope(
        crate::codec::drops::Seam {
            direction: crate::codec::drops::Direction::Response,
            ingress: "anthropic",
            egress: "openai",
        },
        || {
            super::super::anthropic::anthropic_writer()
                .write_response(crate::codec::test_host::HOST, &ir)
        },
    );
    assert_eq!(dropped, vec!["citations"]);
    assert_eq!(out["content"].as_array().unwrap().len(), 1);
    assert_eq!(out["content"][0]["text"], "See the source for details.");
    assert!(out["content"][0]["citations"].is_null());
    assert_eq!(out["usage"]["input_tokens"], 3);
    assert_eq!(out["usage"]["output_tokens"], 4);
    assert_eq!(out["stop_reason"], "end_turn");
}

#[test]
fn response_blank_tool_call_id_is_synthesized() {
    let body = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_COMPLETION, "created": 1u64, "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}},
                {"type": "function", "function": {"name": "get_time", "arguments": "{}"}}
            ]},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let ids: Vec<&str> = ir
        .content
        .iter()
        .filter_map(|b| match b {
            IrBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both tool calls must be read: {:?}",
        ir.content
    );
    for id in &ids {
        assert!(!id.is_empty(), "a synthesized id must never be blank");
        assert!(
            id.starts_with("call_"),
            "synthesized id must be call_…: {id}"
        );
    }
    assert_ne!(ids[0], ids[1], "distinct tool calls must get distinct ids");
}

#[test]
fn openai_write_drops_thinking_observably() {
    let resp = crate::codec::ir::IrResponse {
        content: vec![
            IrBlock::Thinking {
                text: "let me reason about this".to_string(),
                signature: None,
                redacted: false,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            text_block("the answer"),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        ..Default::default()
    };

    let (cap, cap_subscriber) = warnings();
    let sub = cap_subscriber.clone();
    let out = tracing::dispatcher::with_default(&sub, || {
        openai_writer().write_response(crate::codec::test_host::HOST, &resp)
    });

    assert_eq!(out["choices"][0]["message"]["content"], "the answer");
    assert!(
        !out.to_string().contains("let me reason"),
        "reasoning text must not leak onto the OpenAI wire: {out}"
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("thinking"))
            || diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains("reasoning")),
        "dropping a thinking block on OpenAI egress must warn: {:?}",
        diagnostic_texts(&cap)
    );
}

#[test]
fn response_object_tool_arguments_preserved() {
    let body = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_COMPLETION, "created": 1u64, "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": {"city": "SF", "unit": "c"}}}
            ]},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    let input = ir
        .content
        .iter()
        .find_map(|b| match b {
            IrBlock::ToolUse { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a ToolUse block");
    assert_eq!(input.get("city").and_then(|v| v.as_str()), Some("SF"));
    assert_eq!(input.get("unit").and_then(|v| v.as_str()), Some("c"));
}

#[test]
fn response_array_content_image_drop_warns() {
    let body = serde_json::json!({
        "id": "chatcmpl-x", "object": OBJ_COMPLETION, "created": 1u64, "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "here you go"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]},
            "finish_reason": FINISH_STOP
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    });
    let (cap, cap_subscriber) = warnings();
    let sub = cap_subscriber.clone();
    let ir = tracing::dispatcher::with_default(&sub, || {
        OpenAiReader
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read_response")
    });

    assert!(ir.content.iter().any(|b| matches!(b, IrBlock::Text { .. })));
    assert!(!ir
        .content
        .iter()
        .any(|b| matches!(b, IrBlock::Image { .. })));
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("image")),
        "dropping an image response part must warn: {:?}",
        diagnostic_texts(&cap)
    );
}

#[test]
fn stream_usage_subbuckets_match_buffered() {
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();

    let events = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-x", "object": OBJ_CHUNK, "created": 1u64, "model": "gpt-4o",
            "choices": [],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 50,
                "prompt_tokens_details": {"audio_tokens": 7},
                "completion_tokens_details": {
                    "reasoning_tokens": 11, "audio_tokens": 3,
                    "accepted_prediction_tokens": 5, "rejected_prediction_tokens": 2
                }
            }
        }),
        &mut st,
    );
    let detail = events
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage.detail.clone()),
            _ => None,
        })
        .expect("a trailing MessageDelta carrying usage");
    assert_eq!(detail.reasoning_tokens, Some(11));
    assert_eq!(detail.input_audio_tokens, Some(7));
    assert_eq!(detail.output_audio_tokens, Some(3));
    assert_eq!(detail.accepted_prediction_tokens, Some(5));
    assert_eq!(detail.rejected_prediction_tokens, Some(2));
}

fn plain_ir_response(
    logprobs: Vec<crate::codec::ir::IrTokenLogprob>,
) -> crate::codec::ir::IrResponse {
    crate::codec::ir::IrResponse {
        logprobs,
        content: vec![text_block("Hello there!")],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("chatcmpl-1".to_string()),
        created: Some(1),
        ..Default::default()
    }
}

#[test]
fn write_response_emits_required_null_logprobs_and_refusal_when_backend_carried_none() {
    let out = openai_writer().write_response(
        crate::codec::test_host::HOST,
        &plain_ir_response(Vec::new()),
    );
    let choice = out["choices"][0].as_object().expect("choice is an object");
    assert_eq!(
        choice.get("logprobs"),
        Some(&serde_json::Value::Null),
        "choices[].logprobs is required by the spec: JSON null when none were produced: {out}"
    );
    let message = choice["message"].as_object().expect("message is an object");
    assert_eq!(
        message.get("refusal"),
        Some(&serde_json::Value::Null),
        "choices[].message.refusal is required by the spec: JSON null when the model did not \
         refuse: {out}"
    );

    assert_eq!(message["content"], serde_json::json!("Hello there!"));
}

#[test]
fn write_response_passes_carried_logprobs_through_instead_of_null() {
    let carried = vec![crate::codec::ir::IrTokenLogprob {
        token: "Hi".into(),
        logprob: -0.1,
        bytes: None,
        bytes_null: false,
        top: vec![],
    }];
    let out =
        openai_writer().write_response(crate::codec::test_host::HOST, &plain_ir_response(carried));
    assert_eq!(
        out["choices"][0]["logprobs"]["content"][0]["token"],
        serde_json::json!("Hi"),
        "a backend that produced logprobs must reach the caller as the object, not null: {out}"
    );
    assert_eq!(
        out["choices"][0]["message"]["refusal"],
        serde_json::Value::Null
    );
}

#[test]
fn write_response_tool_call_only_turn_still_carries_refusal_and_logprobs() {
    let mut resp = plain_ir_response(Vec::new());
    resp.content = vec![IrBlock::ToolUse {
        thought_signature: None,
        id: "call_1".to_string(),
        name: "get_weather".to_string(),
        input: serde_json::json!({"city": "Paris"}),
        cache_control: None,
    }];
    resp.stop_reason = Some(crate::codec::ir::IrStopReason::ToolUse);
    let out = openai_writer().write_response(crate::codec::test_host::HOST, &resp);
    let message = &out["choices"][0]["message"];
    assert_eq!(message["content"], serde_json::Value::Null);
    assert_eq!(message["refusal"], serde_json::Value::Null);
    assert_eq!(message["tool_calls"][0]["id"], serde_json::json!("call_1"));
    assert_eq!(out["choices"][0]["logprobs"], serde_json::Value::Null);
}

/// An OpenAI-compatible upstream (OpenAI, Azure, vLLM, OpenRouter) that fails MID-STREAM emits an
#[test]
fn test_stream_inline_error_chunk_surfaces_ir_error() {
    let reader = OpenAiReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let content = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "id": "chatcmpl-x",
            "object": OBJ_CHUNK,
            "created": 1,
            "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hel"}}]
        }),
        &mut state,
    );
    assert!(
        !content.is_empty(),
        "the content chunk must decode normally: {content:?}"
    );

    let err_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "error": {
                "message": "The server had an error while processing your request.",
                "type": ERR_TYPE_SERVER_ERROR,
                "code": serde_json::Value::Null
            }
        }),
        &mut state,
    );
    match err_evs.as_slice() {
        [IrStreamEvent::Error(err)] => {
            assert_eq!(
                err.class,
                StatusClass::ServerError,
                "an upstream server_error must classify as a TRANSIENT server fault so the breaker \
                 records it: {err_evs:?}"
            );
            assert_eq!(
                err.provider_signal.as_deref(),
                Some(ERR_TYPE_SERVER_ERROR),
                "the upstream error type must ride through as the provider signal: {err_evs:?}"
            );
        }
        other => panic!("expected exactly one IrStreamEvent::Error, got {other:?}"),
    }
}

/// The inline error's class is derived from the upstream `type`/`code`, not hardcoded: a
#[test]
fn test_stream_inline_error_class_derivation() {
    let cases: [(serde_json::Value, StatusClass); 4] = [
        (
            serde_json::json!({"error": {"type": ERR_TYPE_RATE_LIMIT, "message": "slow down"}}),
            StatusClass::RateLimit,
        ),
        (
            serde_json::json!({"error": {"type": ERR_TYPE_AUTHENTICATION, "message": "bad key"}}),
            StatusClass::Auth,
        ),
        (
            serde_json::json!({"error": {
                "type": ERR_TYPE_INVALID_REQUEST,
                "code": crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH,
                "message": "too long"
            }}),
            StatusClass::ContextLength,
        ),
        (
            serde_json::json!({"error": {"message": "something went wrong"}}),
            StatusClass::ServerError,
        ),
    ];
    for (chunk, expect) in cases {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let evs = OpenAiReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut state,
        );
        match evs.as_slice() {
            [IrStreamEvent::Error(err)] => {
                assert_eq!(err.class, expect, "wrong class for {chunk}: {evs:?}")
            }
            other => panic!("expected one Error for {chunk}, got {other:?}"),
        }
    }
}

/// End-to-end money path: an Anthropic client relayed onto an OpenAI upstream that dies mid-stream
#[test]
fn test_inline_error_chunk_reaches_anthropic_ingress_as_an_error_frame() {
    let mut t = crate::codec::proto_stream::StreamTranslate::new("anthropic", "openai")
        .expect("anthropic-ingress / openai-egress translator");

    let mut out = t.feed(crate::codec::test_host::HOST,
        br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"hel"}}]}

"#,
    );
    out.extend_from_slice(&t.feed(
        crate::codec::test_host::HOST,
        br#"data: {"error":{"message":"upstream exploded","type":"server_error"}}

"#,
    ));
    out.extend_from_slice(&t.finish(crate::codec::test_host::HOST));
    let wire = String::from_utf8_lossy(&out).to_string();

    assert!(
        t.terminal_error().is_some(),
        "a mid-stream upstream error must set terminal_error so the stream is NOT billed as a \
         completed request: {wire}"
    );
    assert!(
        wire.contains("event: error"),
        "the Anthropic client must receive a native error frame, not a silent success: {wire}"
    );
}

#[test]
fn status_word_golden() {
    let golden: [(StatusClass, &str); 9] = [
        (StatusClass::RateLimit, "rate_limit_error"),
        (StatusClass::Overloaded, "server_error"),
        (StatusClass::ServerError, "server_error"),
        (StatusClass::Timeout, "server_error"),
        (StatusClass::Network, "server_error"),
        (StatusClass::Auth, "authentication_error"),
        (StatusClass::Billing, "insufficient_quota"),
        (StatusClass::ClientError, "invalid_request_error"),
        (StatusClass::ContextLength, "invalid_request_error"),
    ];
    for (class, word) in golden {
        let ev = IrStreamEvent::Error(CanonicalSignal {
            class,
            provider_signal: None,
            retry_after: None,
        });
        let (_, chunk) = openai_writer()
            .write_response_event(crate::codec::test_host::HOST, &ev)
            .expect("error emits a chunk");
        assert_eq!(
            chunk["error"]["type"],
            serde_json::json!(word),
            "class={class:?}"
        );
    }
}

/// A `custom` tool call in the conversation history (a type the IR does not model) is skipped by
#[test]
fn a_custom_tool_call_in_history_is_skipped_not_refused() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_c", "type": "custom", "custom": {"name": "grammar", "input": "x = 1"}},
                {"id": "call_f", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_c", "content": "ok"}
        ]
    });
    let ir = OpenAiReader
        .read_request(&body)
        .expect("a custom tool call is not a refusal");
    let uses: Vec<&str> = ir
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            IrBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(uses, ["call_f"]);
    let answer = serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o",
        "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {"role": "assistant",
            "content": null, "tool_calls": [
                {"id": "call_c", "type": "custom", "custom": {"name": "grammar", "input": "x"}}
            ]}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    assert!(OpenAiReader
        .read_response(crate::codec::test_host::HOST, &answer)
        .is_ok());
    let malformed = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "assistant", "tool_calls": [{"id": "call_x", "type": "function"}]}]
    });
    assert!(OpenAiReader.read_request(&malformed).is_err());
}

#[test]
fn system_turn_of_unmodelled_parts_gets_no_empty_text() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": [{"type": "some_future_part", "x": 1}]},
            {"role": "user", "content": "hi"}
        ]
    });
    let ir = OpenAiReader.read_request(&body).expect("parses");
    assert!(ir.system.is_empty(), "{:?}", ir.system);

    let empty_turn = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "system", "content": []}, {"role": "user", "content": "hi"}]
    });
    let ir = OpenAiReader.read_request(&empty_turn).expect("parses");
    assert_eq!(
        ir.system.len(),
        1,
        "the caller's own empty system turn is kept"
    );
}
