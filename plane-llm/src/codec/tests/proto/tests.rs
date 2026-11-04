use super::*;
use busbar_contract::http::StatusCode;

/// The cached streaming-CT set aggregated from the writer vtable must be exactly the SSE +
/// AWS-event-stream pair the per-request sweep produced — the `OnceLock` only memoizes it.
#[test]
fn test_streaming_content_types_cached_set() {
    let got = streaming_content_types();
    let mut want = ["text/event-stream", "application/vnd.amazon.eventstream"];
    want.sort_unstable();
    assert_eq!(
        got, want,
        "cached streaming CT set must be exactly the SSE + amazon-eventstream pair"
    );
}

/// The cached array-stream shim-key set must be exactly Gemini's `GEMINI_JSON_ARRAY_SHIM_KEY`
#[test]
fn test_array_stream_shim_keys_cached_set() {
    assert_eq!(
        array_stream_shim_keys(),
        [crate::codec::gemini::GEMINI_JSON_ARRAY_SHIM_KEY],
        "cached shim-key set must be exactly the gemini json-array key"
    );
}

/// Regression for the conformance bug where `find_frame_terminator` reported `term_len = 3`
#[test]
fn test_find_frame_terminator_crlf_reports_four_bytes() {
    let buf = b"data: x\r\n\r\n";
    let (offset, len) = find_frame_terminator(buf).expect("CRLF terminator must be found");

    assert_eq!(
        offset, 7,
        "offset must index the first terminator byte (\\r)"
    );
    assert_eq!(
        len, 4,
        "CRLF terminator length must be 4 (\\r\\n\\r\\n), not 3"
    );
    assert_eq!(&buf[offset..], b"\r\n\r\n");

    assert_eq!(offset + len, buf.len());
}

/// The LF-only blank-line terminator (`\n\n`) must stay 2 bytes, anchored at the first `\n`.
#[test]
fn test_find_frame_terminator_lf_reports_two_bytes() {
    let buf = b"data: x\n\n";
    let (offset, len) = find_frame_terminator(buf).expect("LF terminator must be found");
    assert_eq!(offset, 7, "offset must index the first `\\n`");
    assert_eq!(len, 2, "LF terminator length must be 2 (\\n\\n)");
    assert_eq!(&buf[offset..], b"\n\n");
    assert_eq!(offset + len, buf.len());
}

/// Two adjacent CRLF frames must split at exactly the documented boundary, so the second
/// frame begins cleanly (no stray leading `\r` and no missing byte).
#[test]
fn test_find_frame_terminator_crlf_frame_split_is_clean() {
    let buf = b"data: x\r\n\r\ndata: y\r\n\r\n";
    let (offset, len) = find_frame_terminator(buf).expect("first CRLF terminator");
    let end = offset + len;
    assert_eq!(
        &buf[..end],
        b"data: x\r\n\r\n",
        "first frame incl. terminator"
    );
    assert_eq!(
        &buf[end..],
        b"data: y\r\n\r\n",
        "remainder is the next frame verbatim"
    );
}

/// The default `ProtocolWriter::write_error` (the only impl — no per-protocol
/// overrides yet) must produce valid JSON carrying the message and the `kind` as `error.type`,
/// so error bodies stay decodable before per-protocol envelopes land. (Content-type is a
/// caller concern; the doc contract says `application/json` for all protocols.)
#[test]
fn test_write_error_default_envelope_is_valid_json() {
    let writer: Box<dyn ProtocolWriter> = Box::new(OpenAiWriter);
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        404,
        "not_found",
        "model 'x' not found",
    );

    let serialized = serde_json::to_string(&v).expect("write_error output must serialize");
    let reparsed: serde_json::Value =
        serde_json::from_str(&serialized).expect("write_error output must be valid JSON");
    assert_eq!(
        reparsed["error"]["message"],
        serde_json::json!("model 'x' not found")
    );
    assert_eq!(reparsed["error"]["type"], serde_json::json!("not_found"));
}

/// Conformance (`synth_anthropic_request_id`): the synthesized `request-id` header value must
/// carry the native Anthropic shape (`req_01` prefix + non-empty token) so the official SDK reads
/// a well-formed `Message._request_id` / `APIError.request_id`.
#[test]
fn test_synth_anthropic_request_id_is_well_formed() {
    let id = synth_anthropic_request_id(crate::codec::test_host::HOST)
        .expect("entropy available in test");
    assert!(
        id.starts_with("req_01"),
        "anthropic request-id must carry the native req_01 prefix; got {id}"
    );

    assert_eq!(
        id.len(),
        30,
        "anthropic request-id must be exactly 30 chars to match native; got {} ({id})",
        id.len()
    );

    let token = &id["req_01".len()..];
    assert_eq!(
        token.len(),
        24,
        "token must be 24 base62 chars; got {token}"
    );
    assert!(
        token.bytes().all(|b| b.is_ascii_alphanumeric()),
        "the token must be base62 (alphanumeric); got {token}"
    );

    let id2 = synth_anthropic_request_id(crate::codec::test_host::HOST)
        .expect("entropy available in test");
    assert_ne!(id, id2, "successive ids must differ");
}

/// Correctness (`synth_anthropic_request_id`): the two 9-byte (72-bit) draws folded to 12
/// base62 digits via repeated `% 62` / `/= 62` are NOT uniform — `62^12 ≈ 3.226e21 < 2^72 ≈
/// 4.722e21`, so the map cannot be injective.
#[test]
fn synth_anthropic_request_id_msd_is_uniform() {
    const N: usize = 4000;
    let mut low = 0usize;
    let mut total = 0usize;
    for _ in 0..N {
        let id = synth_anthropic_request_id(crate::codec::test_host::HOST)
            .expect("entropy available in test");
        let token = &id["req_01".len()..];
        let bytes = token.as_bytes();
        for &msd in &[bytes[0], bytes[12]] {
            total += 1;
            if crate::codec::dialect::BASE62_ALPHABET[..29].contains(&msd) {
                low += 1;
            }
        }
    }
    let frac = low as f64 / total as f64;

    assert!(
        (0.468 - 0.04..=0.468 + 0.04).contains(&frac),
        "token[0]/token[12] must be uniform over the alphabet's first 29 chars \
         (~46.8% +/- 4%); got {:.4} over {total} draws — non-uniform MSD is a fingerprintable tell",
        frac
    );
}

/// Conformance (`GeminiJsonArrayFramer::finish_with_error`): the truncation error element must
/// carry NO busbar-internal vocabulary ("upstream").
#[test]
fn test_gemini_truncation_error_carries_no_internal_vocabulary() {
    let mut f = GeminiJsonArrayFramer::new();

    let huge = vec![b'x'; GeminiJsonArrayFramer::MAX_BUF + 16];
    let mut pre = Vec::from(&b"data: {\"k\":\""[..]);
    pre.extend_from_slice(&huge);
    let _ = f.feed(&pre);
    let tail = f.finish();
    let body = String::from_utf8_lossy(&tail);
    assert!(
        !body.to_lowercase().contains("upstream"),
        "the truncation error must NOT contain the busbar-internal word 'upstream': {body}"
    );
    assert!(
        body.contains("Internal error encountered."),
        "the truncation error must mirror Gemini's own 500 body text: {body}"
    );
}

/// Every protocol's writer must produce a non-empty, valid-JSON probe body that carries the
/// requested model (or, for path-model protocols like Gemini/Bedrock, at least valid JSON) —
/// this is what the active health prober sends.
#[test]
fn test_probe_body_valid_for_all_protocols() {
    for name in [
        "anthropic",
        "openai",
        "gemini",
        "bedrock",
        "responses",
        "cohere",
    ] {
        let proto = protocol_for(name).unwrap();
        let body = proto
            .writer()
            .probe_body(crate::codec::test_host::HOST, "my-model");
        assert!(!body.is_empty(), "{name}: probe body must be non-empty");
        let v: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{name}: invalid JSON: {e}"));
        assert!(v.is_object(), "{name}: probe body must be a JSON object");
    }
}

/// `requires_max_tokens()` must be true exactly for the protocols whose APIs hard-reject a
/// request lacking `max_tokens` (Anthropic Messages) and false for the rest — including Bedrock,
/// which defaults maxTokens when omitted.
#[test]
fn test_requires_max_tokens_per_protocol() {
    for (name, want) in [
        ("anthropic", true),
        ("bedrock", false),
        ("openai", false),
        ("gemini", false),
        ("responses", false),
        ("cohere", false),
    ] {
        assert_eq!(
            crate::codec::decl_of(name).is_some_and(|d| d.requires_max_tokens),
            want,
            "{name}: requires_max_tokens mismatch"
        );
    }
}

/// OpenAI-compatible reasoning models put the chain-of-thought in `reasoning_content`; it must
/// map to a Thinking block (ahead of the answer) so it survives translation to Anthropic.
#[test]
fn test_openai_reasoning_content_maps_to_thinking() {
    let body = serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "reasoning_content": "step 1: think; step 2: answer",
                "content": "the answer"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert!(
        matches!(ir.content.first(), Some(crate::codec::ir::IrBlock::Thinking { text, .. }) if text == "step 1: think; step 2: answer"),
        "first block should be the reasoning as a Thinking block"
    );
    assert!(
        ir.content.iter().any(
            |b| matches!(b, crate::codec::ir::IrBlock::Text { text, .. } if text == "the answer")
        ),
        "the answer text should follow"
    );

    let wire = anthropic_writer().write_response(crate::codec::test_host::HOST, &ir);
    let blocks = wire.get("content").and_then(|c| c.as_array()).unwrap();
    assert!(
        blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking")),
        "Anthropic output should contain a thinking block"
    );
}

/// Streaming reasoning: `delta.reasoning_content` must open a Thinking block at index 0 and
/// close it before the text block (which shifts to index 1).
#[test]
fn test_openai_streaming_reasoning_blocks() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let mut ev = Vec::new();
    ev.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"choices":[{"delta":{"reasoning_content":"mulling"}}]}),
        &mut st,
    ));
    ev.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"choices":[{"delta":{"content":"answer"}}]}),
        &mut st,
    ));
    ev.extend(reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
            &mut st,
        ));

    let think_start = ev.iter().position(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        )
    });
    let think_delta = ev.iter().any(|e| matches!(e, IrStreamEvent::BlockDelta { index: 0, delta: IrDelta::ThinkingDelta(t) } if t == "mulling"));
    let think_stop = ev
        .iter()
        .position(|e| matches!(e, IrStreamEvent::BlockStop { index: 0 }));
    let text_start = ev.iter().position(|e| {
        matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 1,
                block: IrBlockMeta::Text,
                refusal: _,
            }
        )
    });
    let text_delta = ev.iter().any(|e| matches!(e, IrStreamEvent::BlockDelta { index: 1, delta: IrDelta::TextDelta(t) } if t == "answer"));

    assert!(
        think_start.is_some() && think_delta,
        "reasoning opens a Thinking block at index 0"
    );
    assert!(
        text_start.is_some() && text_delta,
        "text opens at index 1 after reasoning"
    );

    let think_stop = think_stop.expect(
        "the thinking block must emit a BlockStop at index 0; without it an ingress accumulator \
         leaves the thinking block open across the text block",
    );
    let text_start = text_start.expect("text opens a BlockStart at index 1");
    assert!(
        think_stop < text_start,
        "the thinking block must close before the text block opens"
    );
}

/// Regression: a normal (no-reasoning) OpenAI stream keeps text at index 0 (offset unchanged).
#[test]
fn test_openai_streaming_no_reasoning_text_index_zero() {
    use crate::codec::ir::{IrBlockMeta, IrStreamEvent};
    let reader = OpenAiReader;
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"choices":[{"delta":{"content":"hi"}}]}),
        &mut st,
    );
    assert!(
        ev.iter().any(|e| matches!(
            e,
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Text,
                refusal: _,
            }
        )),
        "without reasoning, text stays at index 0"
    );
}

fn rich_fixture() -> serde_json::Value {
    serde_json::json!({
        "system": [{"type": "text", "text": "You are a helpful assistant.", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "What is the weather?"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}}]},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "I need to analyze the weather...", "signature": "sig_abc123xyz"}, {"type": "tool_use", "id": "tool_1", "name": "get_weather", "input": {"location": "San Francisco"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": [{"type": "text", "text": "Sunny, 72°F"}]}]}
        ],
        "tools": [{"name": "get_weather", "description": "Get weather for a location", "input_schema": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}}],
        "max_tokens": 4096,
        "temperature": 0.7,
        "stream": true,
        "top_p": 0.95
    })
}

#[test]
fn test_openai_tool_schema_translates_to_anthropic() {
    let openai_body = serde_json::json!({
        "model": "x",
        "max_tokens": 200,
        "messages": [{"role": "user", "content": "weather in Paris?"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        }]
    });
    let ir = OpenAiReader
        .read_request(&openai_body)
        .expect("openai read_request");
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(
        ir.tools[0].name, "get_weather",
        "tool name (nested under function)"
    );
    assert_eq!(
        ir.tools[0].input_schema["properties"]["city"]["type"], "string",
        "parameters schema must be read into IrTool.input_schema"
    );

    let anthropic = anthropic_writer().write_request(crate::codec::test_host::HOST, &ir);
    let tools = anthropic.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
    assert!(
        !tools[0]["input_schema"].is_null(),
        "Anthropic tool input_schema must not be null (caused the 422)"
    );
    assert_eq!(
        tools[0]["input_schema"]["properties"]["city"]["type"], "string",
        "the full JSON schema must survive OpenAI → Anthropic translation"
    );
}

#[test]
fn test_roundtrip_identity() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();
    let j = rich_fixture();
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        roundtrip, j,
        "round-trip must be byte-identical on representable subset"
    );
}

#[test]
fn test_signature_verbatim() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();
    let j = rich_fixture();
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    let mut found_thinking = false;
    for msg in &ir.messages {
        if msg.role == crate::codec::ir::IrRole::Assistant {
            for block in &msg.content {
                if let crate::codec::ir::IrBlock::Thinking {
                    text: _, signature, ..
                } = block
                {
                    found_thinking = true;
                    assert_eq!(signature.as_deref(), Some("sig_abc123xyz"));
                }
            }
        }
    }
    assert!(found_thinking);
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

    let mut wrote_thinking = false;
    if let Some(msgs) = roundtrip.get("messages").and_then(|v| v.as_array()) {
        for msg_val in msgs {
            if let Some(content_arr) = msg_val.get("content").and_then(|v| v.as_array()) {
                for block_val in content_arr {
                    if let Some(block_obj) = block_val.as_object() {
                        if block_obj.get("type").and_then(|t| t.as_str()) == Some("thinking") {
                            wrote_thinking = true;
                            assert_eq!(
                                block_obj.get("signature").and_then(|s| s.as_str()),
                                Some("sig_abc123xyz")
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(
        wrote_thinking,
        "the writer must emit the assistant thinking block back, or its signature cannot survive: \
         {roundtrip}"
    );
}

#[test]
fn test_cache_control_preserved() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();
    let j = rich_fixture();
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    assert!(!ir.system.is_empty());

    let crate::codec::ir::IrBlock::Text {
        text: _,
        cache_control,
        citations: _,
        refusal: _,
    } = &ir.system[0]
    else {
        panic!(
            "the anthropic reader must carry the system prompt as a Text block, got {:?}",
            ir.system[0]
        );
    };
    assert!(cache_control.is_some());
    match cache_control.as_ref().unwrap().kind {
        crate::codec::ir::CacheKind::Ephemeral => {}
    };
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);
    let system_arr = roundtrip
        .get("system")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!("the writer must emit `system` as an array to carry cache_control: {roundtrip}")
        });
    let first_block = system_arr
        .first()
        .unwrap_or_else(|| panic!("the written `system` array must not be empty: {roundtrip}"));
    assert!(
        first_block
            .as_object()
            .expect("system block object")
            .contains_key("cache_control"),
        "the system prompt's cache breakpoint must survive the write: {roundtrip}"
    );
}

/// An Anthropic `cache_control`
#[test]
fn cache_breakpoints_gated_by_lane_capability_on_bedrock() {
    let anthropic = crate::codec::proto_codec::protocol_for("anthropic").unwrap();
    let bedrock = crate::codec::proto_codec::protocol_for("bedrock").unwrap();
    assert!(crate::codec::decl_of("bedrock").is_some_and(|d| d.cache_markers_model_gated));
    assert!(crate::codec::decl_of("anthropic").is_some_and(|d| !d.cache_markers_model_gated));

    let body = serde_json::json!({
        "model": "claude", "max_tokens": 64,
        "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
        ]}],
        "tools": [{"name": "t", "input_schema": {"type": "object"},
                   "cache_control": {"type": "ephemeral"}}]
    });

    let prep = |allowed: bool| crate::codec::egress_prep::EgressPrep {
        thought_signature_fill: false,
        ingress_protocol: "anthropic",
        egress_requires_max_tokens: false,
        lane_default_max_tokens: None,
        global_default_max_tokens: 4096,
        reasoning_allowed: false,
        reasoning_budgets: [1024, 4096, 8192, 16384],
        prompt_caching_allowed: allowed,
        cache_control_cap: None,
        lane_caps: Default::default(),
    };
    let contains_cache_point =
        |wire: &serde_json::Value| serde_json::to_string(wire).unwrap().contains("cachePoint");

    let ir = anthropic.reader().read_request(&body).unwrap();
    let mut req = ir;
    crate::codec::chat_handle::chat_prepare_for_egress(&mut req, &prep(false));
    let ir = req;
    let wire = bedrock
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        !contains_cache_point(&wire),
        "an unasserted lane must never receive a cachePoint (Nova 400s on it): {wire}"
    );

    let ir = anthropic.reader().read_request(&body).unwrap();
    let mut req = ir;
    crate::codec::chat_handle::chat_prepare_for_egress(&mut req, &prep(true));
    let ir = req;
    let wire = bedrock
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        contains_cache_point(&wire),
        "an asserted lane keeps its cache markers: {wire}"
    );
}

#[test]
fn test_extra_passthrough() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();
    let j = rich_fixture();
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");

    assert!(!ir.extra.contains_key("top_p"));
    assert!(ir.top_p.is_some());
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(roundtrip.as_object().unwrap().contains_key("top_p"));
}

#[test]
fn test_cross_protocol_openai_top_p_to_anthropic() {
    let body = serde_json::json!({
        "model":"gpt-x",
        "messages":[{"role":"user","content":"hi"}],
        "top_p":0.81
    });
    let mut ir = OpenAiReader.read_request(&body).expect("openai parses");
    assert_eq!(ir.top_p, Some(0.81));
    ir.extra.clear();
    let out = anthropic_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("top_p").and_then(|v| v.as_f64()),
        Some(0.81),
        "openai top_p must translate to anthropic top_p across the seam; got {out}"
    );
}

#[test]
fn test_cross_protocol_gemini_stop_sequences_to_openai_stop() {
    let body = serde_json::json!({
        "model":"gemini-x",
        "contents":[{"role":"user","parts":[{"text":"hi"}]}],
        "generationConfig":{"stopSequences":["STOP","END"]}
    });
    let mut ir = GeminiReader.read_request(&body).expect("gemini parses");
    assert_eq!(ir.stop, vec!["STOP".to_string(), "END".to_string()]);
    ir.extra.clear();
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("stop"),
        Some(&serde_json::json!(["STOP", "END"])),
        "gemini stopSequences must translate to openai stop across the seam; got {out}"
    );
}

#[test]
fn test_cross_protocol_anthropic_top_k_to_gemini() {
    let body = serde_json::json!({
        "model":"claude-x",
        "messages":[{"role":"user","content":"hi"}],
        "max_tokens":16,
        "top_k":40
    });
    let mut ir = AnthropicReader
        .read_request(&body)
        .expect("anthropic parses");
    assert_eq!(ir.top_k, Some(40));
    ir.extra.clear();
    let writer = GeminiWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.pointer("/generationConfig/topK")
            .and_then(|v| v.as_u64()),
        Some(40),
        "anthropic top_k must translate to gemini generationConfig.topK; got {out}"
    );
}

#[test]
fn test_cross_protocol_top_k_dropped_for_openai_target() {
    let body = serde_json::json!({
        "model":"claude-x",
        "messages":[{"role":"user","content":"hi"}],
        "max_tokens":16,
        "top_k":40
    });
    let mut ir = AnthropicReader
        .read_request(&body)
        .expect("anthropic parses");
    ir.extra.clear();
    let out = openai_writer().write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("top_k").is_none() && out.get("k").is_none(),
        "OpenAI has no top_k knob; the writer must not synthesize one; got {out}"
    );
}

#[test]
fn test_registry_resolves_anthropic() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    assert_eq!(protocol.name_static(), "anthropic");
    assert_eq!(protocol.writer().upstream_path(), "/v1/messages");

    assert!(crate::codec::proto_codec::protocol_for("nonexistent").is_none());
}

#[test]
fn test_reader_classify_behavior() {
    let protocol =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    let reader = protocol.reader();

    let signal = reader.classify(StatusCode::TOO_MANY_REQUESTS, b"{}");
    assert_eq!(signal.class, StatusClass::RateLimit);

    let signal = reader.classify(StatusCode::UNAUTHORIZED, b"{}");
    assert_eq!(signal.class, StatusClass::Auth);

    let signal = reader.classify(StatusCode::SERVICE_UNAVAILABLE, b"{}");
    assert_eq!(signal.class, StatusClass::ServerError);
}

#[test]
fn test_irerror_bridge() {
    let ir_error: IrError = IrError {
        class: StatusClass::Billing,
        provider_signal: Some("test".to_string()),
        retry_after: None,
    };

    assert_eq!(
        ir_error.class.disposition(),
        busbar_contract::upstream::Disposition::HardDown
    );
}

#[test]
fn test_stream_roundtrip_identity() {
    let reader = AnthropicReader;
    let writer = AnthropicWriter;

    let data = serde_json::json!({
        "message": {
            "role": "assistant",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 15
            }
        }
    });
    let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_start", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        let (et, out) = writer
            .write_response_event(crate::codec::test_host::HOST, &e)
            .expect("writes message_start");
        assert_eq!(et, "message_start");
        let msg = out.get("message").expect("message object");
        assert!(
            msg.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .starts_with("msg_"),
            "synthesized id must be msg_-prefixed: {out}"
        );
        assert_eq!(msg.get("type").and_then(|v| v.as_str()), Some("message"));
        assert_eq!(msg.get("role").and_then(|v| v.as_str()), Some("assistant"));
        assert!(msg.get("content").and_then(|c| c.as_array()).is_some());
        assert!(msg.get("stop_reason").map(|v| v.is_null()).unwrap_or(false));
        assert!(msg
            .get("stop_sequence")
            .map(|v| v.is_null())
            .unwrap_or(false));
        assert_eq!(
            msg.get("usage").and_then(|u| u.get("input_tokens")),
            Some(&serde_json::json!(10))
        );
    }

    let data = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {
            "type": "tool_use",
            "id": "tool_123",
            "name": "get_weather",
            "input": {},
            "caller": {"type": "direct"}
        }
    });
    let ev =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_start", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_start".to_string(), data))
        );
    }

    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {
            "type": "text_delta",
            "text": "hello"
        }
    });
    let ev =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_delta".to_string(), data))
        );
    }

    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 1,
        "delta": {
            "type": "thinking_delta",
            "thinking": "I need to think"
        }
    });
    let ev =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_delta".to_string(), data))
        );
    }

    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 2,
        "delta": {
            "type": "input_json_delta",
            "partial_json": "{\"loc"
        }
    });
    let ev =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_delta".to_string(), data))
        );
    }

    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 1,
        "delta": {
            "type": "signature_delta",
            "signature": "sig_abc123xyz"
        }
    });
    let ev =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_delta".to_string(), data))
        );
    }

    let data = serde_json::json!({ "type": "content_block_stop", "index": 0 });
    let ev = reader.read_response_event(crate::codec::test_host::HOST, "content_block_stop", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("content_block_stop".to_string(), data))
        );
    }

    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {
            "container": null,
            "stop_details": null,
            "stop_reason": "end_turn",
            "stop_sequence": null
        },
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 5,
            "cache_read_input_tokens": 15,
            "output_tokens_details": null,
            "server_tool_use": null
        }
    });
    let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_delta", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("message_delta".to_string(), data))
        );
    }

    let data = serde_json::json!({ "type": "message_stop" });
    let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_stop", &data);
    assert!(ev.is_some());
    if let Some(e) = ev {
        assert_eq!(
            writer.write_response_event(crate::codec::test_host::HOST, &e),
            Some(("message_stop".to_string(), data))
        );
    }
}

#[test]
fn test_split_usage_never_collapses() {
    let reader = AnthropicReader;
    let writer = AnthropicWriter;

    let data = serde_json::json!({
        "delta": { "stop_reason": "end_turn" },
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 30,
            "cache_read_input_tokens": 200
        }
    });

    let ev = reader
        .read_response_event(crate::codec::test_host::HOST, "message_delta", &data)
        .expect("should parse");
    if let crate::codec::ir::IrStreamEvent::MessageDelta {
        stop_reason: _,
        usage,
        ..
    } = ev
    {
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, Some(30));
        assert_eq!(usage.cache_read_input_tokens, Some(200));
    } else {
        panic!("expected MessageDelta");
    }

    let roundtrip = writer.write_response_event(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: Some(30),
                cache_read_input_tokens: Some(200),
                detail: crate::codec::ir::IrUsageDetail::default(),
            },
            stop_detail: None,
        },
    );
    assert!(roundtrip.is_some());
    let (_, rt_data) = roundtrip.unwrap();
    assert_eq!(
        rt_data
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64()),
        Some(100)
    );
    assert_eq!(
        rt_data
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64()),
        Some(50)
    );
    assert_eq!(
        rt_data
            .get("usage")
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|v| v.as_u64()),
        Some(30)
    );
    assert_eq!(
        rt_data
            .get("usage")
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64()),
        Some(200)
    );
}

#[test]
fn test_signature_delta_verbatim() {
    let reader = AnthropicReader;
    let writer = AnthropicWriter;

    let sig = "sig_abc123xyz_signature_for_thinking";
    let data = serde_json::json!({
        "index": 0,
        "delta": {
            "type": "signature_delta",
            "signature": sig
        }
    });

    let ev = reader
        .read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data)
        .expect("should parse");
    if let crate::codec::ir::IrStreamEvent::BlockDelta { index: _, delta } = ev {
        if let crate::codec::ir::IrDelta::SignatureDelta(s) = delta {
            assert_eq!(s, sig);
        } else {
            panic!("expected SignatureDelta");
        }
    } else {
        panic!("expected BlockDelta");
    }

    let roundtrip = writer.write_response_event(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::SignatureDelta(sig.to_string()),
        },
    );
    assert!(roundtrip.is_some());
    let (_, rt_data) = roundtrip.unwrap();
    let rt_sig = rt_data
        .get("delta")
        .and_then(|d| d.get("signature"))
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(rt_sig, sig);
}

#[test]
fn test_ping_returns_none() {
    let reader = AnthropicReader;
    let data = serde_json::json!({});
    let result = reader.read_response_event(crate::codec::test_host::HOST, "ping", &data);
    assert!(result.is_none());

    let result =
        reader.read_response_event(crate::codec::test_host::HOST, "unknown_event_type", &data);
    assert!(result.is_none());
}

#[test]
fn test_openai_request_roundtrip_identity() {
    let protocol = crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();

    let j = serde_json::json!({
        "model": "gpt-4",
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": [{"type": "text", "text": "hello"}, {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}]},
            {"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "get_weather", "arguments": "{\"location\":\"San Francisco\"}"}}]},
            {"role": "tool", "tool_call_id": "call_123", "content": "Sunny, 72°F"}
        ],
        "tools": [{"type": "function", "name": "get_weather", "description": "Get weather for a location", "parameters": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}}],
        "max_tokens": 100,
        "temperature": 0.7,
        "stream": true,
        "top_p": 0.95
    });

    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        roundtrip
            .as_object()
            .unwrap()
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        j.get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
    );
    assert_eq!(
        roundtrip.as_object().unwrap().get("max_tokens"),
        j.as_object().unwrap().get("max_tokens")
    );
    assert_eq!(
        roundtrip.as_object().unwrap().get("temperature"),
        j.as_object().unwrap().get("temperature")
    );
    assert_eq!(
        roundtrip.as_object().unwrap().get("stream"),
        j.as_object().unwrap().get("stream")
    );
    assert_eq!(
        roundtrip.as_object().unwrap().get("top_p"),
        j.as_object().unwrap().get("top_p")
    );

    let msgs = roundtrip
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap();
    let written_id = msgs
        .iter()
        .find_map(|m| m.get("tool_calls").and_then(|tc| tc.as_array()))
        .and_then(|tc| tc.first())
        .and_then(|c| c.get("id"))
        .and_then(|i| i.as_str());
    assert_eq!(
        written_id,
        Some("call_123"),
        "tool_call id must round-trip verbatim, not be regenerated"
    );

    let result_ref = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .and_then(|m| m.get("tool_call_id"))
        .and_then(|i| i.as_str());
    assert_eq!(
        result_ref,
        Some("call_123"),
        "tool-result correlation must survive round-trip"
    );
}

fn assert_written_openai_tool_arguments(
    roundtrip: &serde_json::Value,
    expected: &serde_json::Value,
) {
    let messages = roundtrip["messages"]
        .as_array()
        .expect("the writer emits messages");
    let call = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| {
            message
                .get("tool_calls")
                .and_then(|calls| calls.as_array())
                .into_iter()
                .flatten()
        })
        .find(|call| call["id"] == "call_123")
        .expect("the writer emits the assistant's actual tool call");
    assert_eq!(call["type"], "function");
    let function = call["function"]
        .as_object()
        .expect("the emitted tool call has a function");
    assert_eq!(
        function.get("name"),
        Some(&serde_json::json!("get_weather"))
    );
    let arguments = function
        .get("arguments")
        .and_then(|value| value.as_str())
        .expect("the emitted function arguments are a JSON string");
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).expect("arguments parse as JSON");
    assert_eq!(
        &parsed, expected,
        "the writer preserves the parsed arguments"
    );
}

#[test]
fn test_openai_tool_call_arguments_string_to_value() {
    let protocol = crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
    let reader = protocol.reader();
    let writer = protocol.writer();

    let j = serde_json::json!({
        "model": "gpt-4",
        "messages": [
            {"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "get_weather", "arguments": "{\"location\":\"San Francisco\"}"}}]}
        ]
    });

    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");

    let mut found_tool_use = false;
    for msg in &ir.messages {
        if msg.role == crate::codec::ir::IrRole::Assistant {
            for block in &msg.content {
                if let crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } = block
                {
                    found_tool_use = true;
                    assert_eq!(id, "call_123");
                    assert_eq!(name, "get_weather");

                    match input {
                        serde_json::Value::Object(_) => {}
                        _ => panic!("arguments should parse to Object"),
                    }
                }
            }
        }
    }
    assert!(found_tool_use);

    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

    let crate::codec::ir::IrBlock::ToolUse { input, .. } = &ir.messages[0].content[0] else {
        panic!("expected the parsed ToolUse block");
    };
    assert_written_openai_tool_arguments(&roundtrip, input);
}

#[test]
fn test_registry_has_both_protocols() {
    assert!(
        crate::codec::proto_codec::protocol_for("anthropic").is_some(),
        "anthropic should exist"
    );
    assert!(
        crate::codec::proto_codec::protocol_for("openai").is_some(),
        "openai should exist"
    );

    let openai = crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
    assert_eq!(openai.writer().upstream_path(), "/v1/chat/completions");

    let anthropic =
        crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
    assert_eq!(anthropic.writer().upstream_path(), "/v1/messages");
}

#[test]
fn test_protocol_clone_works() {
    let openai_proto = Protocol::openai();
    let cloned_openai = openai_proto.clone();

    assert_eq!(cloned_openai.name_static(), "openai");
    assert_eq!(
        cloned_openai.writer().upstream_path(),
        "/v1/chat/completions"
    );

    let anthropic_proto = Protocol::anthropic();
    let cloned_anthropic = anthropic_proto.clone();

    assert_eq!(cloned_anthropic.name_static(), "anthropic");
    assert_eq!(cloned_anthropic.writer().upstream_path(), "/v1/messages");

    let openai_reader: Box<dyn ProtocolReader> = Box::new(OpenAiReader);
    let _cloned_reader = openai_reader.clone();

    let openai_writer: Box<dyn ProtocolWriter> = Box::new(OpenAiWriter);
    let _cloned_writer = openai_writer.clone();
}

/// Cohere v2 carries the assistant's INTERNAL pre-tool plan in `message.tool_plan`. It must reach
/// the IR — unread, it vanishes on any Cohere→X hop — but as REASONING, not as visible text.
#[test]
fn cohere_read_response_surfaces_tool_plan_as_reasoning_not_visible_text() {
    let body = serde_json::json!({
        "message": {
            "tool_plan": "First I will check the weather.",
            "tool_calls": [{"id": "t1", "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}}]
        },
        "finish_reason": "COMPLETE"
    });
    let ir = CohereReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("cohere read_response");
    assert!(
        matches!(ir.content.first(), Some(crate::codec::ir::IrBlock::Thinking { text, .. }) if text == "First I will check the weather."),
        "tool_plan must be the leading REASONING block, got: {:?}",
        ir.content
    );
    assert!(
        !ir.content.iter().any(|b| matches!(
            b, crate::codec::ir::IrBlock::Text { text, .. } if text.contains("First I will check")
        )),
        "the plan must not ALSO appear as visible text: {:?}",
        ir.content
    );
    assert!(
        ir.content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })),
        "the tool call must still follow the plan"
    );
}

/// Responses retains partial string arguments verbatim. Cohere refuses malformed JSON, while
/// retaining a represented JSON string's original spacing instead of encoding it a second time.
#[test]
fn string_args_writers_emit_raw_tool_args_verbatim() {
    let raw = "not valid json {";
    let ir = crate::codec::ir::IrResponse {
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            thought_signature: None,
            id: "call_1".into(),
            name: "do_it".into(),
            input: serde_json::Value::String(raw.into()),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: None,
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,
        logprobs: Vec::new(),

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let resp_w = ResponsesWriter;
    let resp = resp_w.write_response(crate::codec::test_host::HOST, &ir);
    let resp_args = resp
        .get("output")
        .and_then(|o| o.as_array())
        .expect("output array")
        .iter()
        .find(|it| it.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        .and_then(|it| it.get("arguments"))
        .and_then(|a| a.as_str())
        .expect("responses function_call arguments");
    assert_eq!(
        resp_args, raw,
        "Responses must emit raw string args verbatim"
    );

    let coh_w = CohereWriter;
    assert!(coh_w
        .try_write_response(crate::codec::test_host::HOST, &ir)
        .is_err());
    let valid_raw = " { \"city\" : \"Paris\" } ";
    let mut valid = ir.clone();
    let crate::codec::ir::IrBlock::ToolUse { input, .. } = &mut valid.content[0] else {
        panic!("tool call fixture")
    };
    *input = serde_json::Value::String(valid_raw.into());
    let coh = coh_w
        .try_write_response(crate::codec::test_host::HOST, &valid)
        .unwrap();
    let coh_args = coh
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
        .expect("cohere tool_calls array")
        .first()
        .and_then(|tc| tc.get("function"))
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.as_str())
        .expect("cohere function arguments");
    assert_eq!(
        coh_args, valid_raw,
        "Cohere must emit represented raw string args verbatim"
    );
}

/// Regression: Cohere is a free-form-tool-id
/// ingress with NO canonical prefix, so `crate::codec::proto_codec::native_tool_id_prefix("cohere")` must be `None` (like
/// Gemini).
#[test]
fn cohere_tool_ids_pass_through_verbatim_no_decode() {
    assert_eq!(
        crate::codec::proto_codec::native_tool_id_prefix("cohere"),
        None
    );

    assert_eq!(decode_native_tool_id("cohere", "bb161626364"), None);

    assert_eq!(decode_native_tool_id("cohere", "my-tool-call-7"), None);

    let mut remap = ToolIdRemap::default();
    assert_eq!(remap.native_for("cohere", "call_xyz"), "call_xyz");
    assert_eq!(remap.native_for("cohere", "bb161626364"), "bb161626364");
}

/// CHARACTERIZATION: this passes before and after `write_sse_frame` by construction, since it
/// pins byte-for-byte output rather than a behavior change.
#[test]
fn write_sse_frame_pins_both_framings_byte_for_byte() {
    let data = serde_json::json!({"a": 1});

    let mut out = Vec::new();
    crate::codec::dialect::write_sse_frame(&mut out, "x", &data);
    assert_eq!(out, b"event: x\ndata: {\"a\":1}\n\n");

    let mut out = Vec::new();
    crate::codec::dialect::write_sse_frame(&mut out, "", &data);
    assert_eq!(out, b"data: {\"a\":1}\n\n");
}

#[cfg(test)]
mod ir_property_tests {
    use super::*;

    /// Rich canonical Anthropic fixture with natural values only (0.7, "hello", 10, "call_123").
    fn anthropic_rich_fixture() -> serde_json::Value {
        serde_json::json!({
            "system": [
                {
                    "type": "text",
                    "text": "You are a helpful assistant.",
                    "cache_control": {"type": "ephemeral"}
                }
            ],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "hello"},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
                            }
                        }
                    ]
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "I need to analyze this request carefully...",
                            "signature": "sig_thinking_abc123"
                        },
                        {
                            "type": "tool_use",
                            "id": "call_123",
                            "name": "get_weather",
                            "input": {"location": "San Francisco"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_123",
                            "content": [{"type": "text", "text": "Sunny, 72°F"}],
                            "is_error": false
                        }
                    ]
                }
            ],
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get weather for a location",
                    "input_schema": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            ],
            "max_tokens": 10,
            "temperature": 0.7,
            "stream": true,
            "top_p": 0.95
        })
    }

    #[test]
    fn test_anthropic_request_decode_assertions() {
        let protocol =
            crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
        let reader = protocol.reader();
        let j = anthropic_rich_fixture();

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");

        assert!(!ir.system.is_empty());
        if let crate::codec::ir::IrBlock::Text {
            ref text,
            ref cache_control,
            ref citations,
            refusal: _,
        } = ir.system[0]
        {
            assert_eq!(text, "You are a helpful assistant.");
            assert!(cache_control.is_some());
            match cache_control.as_ref().unwrap().kind {
                crate::codec::ir::CacheKind::Ephemeral => {}
            }
            assert!(citations.is_empty());
        } else {
            panic!("system[0] should be Text block");
        }

        let mut found_assistant = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::Assistant {
                found_assistant = true;
                let mut found_thinking = false;
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::Thinking {
                        text: _,
                        ref signature,
                        ..
                    } = block
                    {
                        found_thinking = true;
                        assert_eq!(signature.as_deref(), Some("sig_thinking_abc123"));
                    }
                }
                assert!(found_thinking);
            }
        }
        assert!(found_assistant);

        let mut found_tool_use = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::Assistant {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        found_tool_use = true;
                        assert_eq!(id, "call_123");
                        assert_eq!(name, "get_weather");
                        match input {
                            serde_json::Value::Object(obj) => {
                                assert_eq!(
                                    obj.get("location"),
                                    Some(&serde_json::json!("San Francisco"))
                                );
                            }
                            _ => panic!("input should be Object"),
                        }
                    }
                }
            }
        }
        assert!(found_tool_use);

        let mut found_image = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::User {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::Image {
                        source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
                        ..
                    } = block
                    {
                        found_image = true;
                        assert_eq!(media_type, "image/png");
                        assert_eq!(data, "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ");
                    }
                }
            }
        }
        assert!(found_image);

        let mut found_tool_result = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::User {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolResult {
                        ref tool_use_id,
                        ref content,
                        ref is_error,
                        ..
                    } = block
                    {
                        found_tool_result = true;
                        assert_eq!(tool_use_id, "call_123");
                        assert!(!content.is_empty());
                        assert!(!*is_error);
                    }
                }
            }
        }
        assert!(found_tool_result);

        assert_eq!(ir.temperature, Some(0.7_f64));

        assert!(!ir.extra.contains_key("top_p"));
        assert_eq!(ir.top_p, Some(0.95_f64));
    }

    #[test]
    fn test_anthropic_request_roundtrip_identity() {
        let protocol =
            crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
        let reader = protocol.reader();
        let writer = protocol.writer();
        let j = anthropic_rich_fixture();

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");

        let roundtrip_json = writer.write_request(crate::codec::test_host::HOST, &ir);
        let rt_ir = reader
            .read_request(&roundtrip_json)
            .expect("read round-trip should succeed");

        assert_eq!(ir, rt_ir, "decoded IR must be identical after round-trip");
    }

    #[test]
    fn test_anthropic_request_empty_minimal() {
        let j = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        });
        let protocol =
            crate::codec::proto_codec::protocol_for("anthropic").expect("anthropic should exist");
        let reader = protocol.reader();
        let writer = protocol.writer();

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");

        assert!(ir.system.is_empty());
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
        if let crate::codec::ir::IrBlock::Text { ref text, .. } = ir.messages[0].content[0] {
            assert_eq!(text, "hi");
        } else {
            panic!("expected Text block");
        }
        assert!(ir.tools.is_empty());
        assert_eq!(ir.max_tokens, None);
        assert_eq!(ir.temperature, None);
        assert!(!ir.stream);

        let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

        let rt_ir = reader
            .read_request(&roundtrip)
            .expect("read round-trip should succeed");
        assert_eq!(ir, rt_ir);
    }

    /// Canonical OpenAI fixture with natural values only.
    fn openai_rich_fixture() -> serde_json::Value {
        serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "hello"},
                        {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
                    ]
                },
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_123",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"San Francisco\"}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_123", "content": "Sunny, 72°F"}
            ],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            ],
            "max_tokens": 100,
            "temperature": 0.7,
            "stream": true,
            "top_p": 0.95
        })
    }

    #[test]
    fn test_openai_request_decode_assertions() {
        let protocol =
            crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
        let reader = protocol.reader();
        let j = openai_rich_fixture();

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");

        assert!(!ir.system.is_empty());
        if let crate::codec::ir::IrBlock::Text { ref text, .. } = ir.system[0] {
            assert_eq!(text, "You are a helpful assistant.");
        } else {
            panic!("system[0] should be Text block");
        }

        let mut found_tool_use = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::Assistant {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolUse { id, name, .. } = block {
                        found_tool_use = true;
                        assert_eq!(id, "call_123", "ToolUse id must be verbatim from input");
                        assert_eq!(name, "get_weather");
                    }
                }
            }
        }
        assert!(found_tool_use);

        let mut found_tool_result = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::Tool {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolResult {
                        ref tool_use_id, ..
                    } = block
                    {
                        found_tool_result = true;
                        assert_eq!(
                            tool_use_id, "call_123",
                            "tool_result correlation must survive"
                        );
                    }
                }
            }
        }
        assert!(found_tool_result);

        let mut found_image = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::User {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::Image {
                        source: crate::codec::ir::IrImageSource::Url(data),
                        ..
                    } = block
                    {
                        found_image = true;
                        assert_eq!(data, "https://example.com/image.png");
                    }
                }
            }
        }
        assert!(found_image);

        assert_eq!(ir.temperature, Some(0.7_f64));
    }

    #[test]
    fn test_openai_tool_call_id_correlation_survives_write() {
        let protocol =
            crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
        let reader = protocol.reader();
        let writer = protocol.writer();
        let j = openai_rich_fixture();

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");
        let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

        let mut saw_assistant_call = false;
        let mut saw_tool_result = false;

        if let Some(msgs) = roundtrip.get("messages").and_then(|v| v.as_array()) {
            for msg_val in msgs {
                if let Some(tc_arr) = msg_val.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc_val in tc_arr {
                        if let Some(id) = tc_val.get("id").and_then(|i| i.as_str()) {
                            saw_assistant_call = true;
                            assert_eq!(id, "call_123", "assistant tool_call id must survive write");
                        }
                    }
                }
            }
        }

        if let Some(msgs) = roundtrip.get("messages").and_then(|v| v.as_array()) {
            for msg_val in msgs {
                if msg_val.get("role").and_then(|r| r.as_str()) == Some("tool") {
                    if let Some(tool_call_id) = msg_val.get("tool_call_id").and_then(|i| i.as_str())
                    {
                        saw_tool_result = true;
                        assert_eq!(
                            tool_call_id, "call_123",
                            "tool message correlation must survive"
                        );
                    } else {
                        panic!("tool message should have tool_call_id");
                    }
                }
            }
        }

        assert!(
            saw_assistant_call,
            "the writer must emit the assistant tool_call with an id — there is no correlation to \
             survive otherwise: {roundtrip}"
        );
        assert!(
            saw_tool_result,
            "the writer must emit the tool-result message — the other half of the correlation: \
             {roundtrip}"
        );
    }

    #[test]
    fn test_openai_arguments_string_to_value_roundtrip() {
        let protocol =
            crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
        let reader = protocol.reader();
        let writer = protocol.writer();

        let j = serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_123",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"San Francisco\",\"unit\":\"celsius\"}"
                            }
                        }
                    ]
                }
            ]
        });

        let ir = reader
            .read_request(&j)
            .expect("read_request should succeed");

        let mut found_tool_use = false;
        for msg in &ir.messages {
            if msg.role == crate::codec::ir::IrRole::Assistant {
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        found_tool_use = true;
                        assert_eq!(id, "call_123");
                        assert_eq!(name, "get_weather");
                        match input {
                            serde_json::Value::Object(obj) => {
                                assert_eq!(
                                    obj.get("location"),
                                    Some(&serde_json::json!("San Francisco"))
                                );
                                assert_eq!(obj.get("unit"), Some(&serde_json::json!("celsius")));
                            }
                            _ => panic!("arguments should parse to Object Value"),
                        }
                    }
                }
            }
        }
        assert!(found_tool_use);

        let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);
        let crate::codec::ir::IrBlock::ToolUse { input, .. } = &ir.messages[0].content[0] else {
            panic!("expected the parsed ToolUse block");
        };
        assert_written_openai_tool_arguments(&roundtrip, input);
    }

    #[test]
    fn test_anthropic_stream_per_event_roundtrip() {
        let reader = AnthropicReader;
        let writer = AnthropicWriter;

        let data = serde_json::json!({
            "message": {
                "role": "assistant",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 15
                }
            }
        });
        let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_start", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            let (et, out) = writer
                .write_response_event(crate::codec::test_host::HOST, &e)
                .expect("writes message_start");
            assert_eq!(et, "message_start");
            let msg = out.get("message").expect("message object");
            assert!(
                msg.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .starts_with("msg_"),
                "synthesized id must be msg_-prefixed: {out}"
            );
            assert_eq!(msg.get("type").and_then(|v| v.as_str()), Some("message"));
            assert_eq!(msg.get("role").and_then(|v| v.as_str()), Some("assistant"));
            assert!(msg.get("content").and_then(|c| c.as_array()).is_some());
            assert!(msg.get("stop_reason").map(|v| v.is_null()).unwrap_or(false));
            assert!(msg
                .get("stop_sequence")
                .map(|v| v.is_null())
                .unwrap_or(false));
            assert_eq!(
                msg.get("usage")
                    .and_then(|u| u.get("cache_read_input_tokens")),
                Some(&serde_json::json!(15))
            );
        }

        let data = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "tool_use",
                "id": "call_123",
                "name": "get_weather",
                "input": {},
                "caller": {"type": "direct"}
            }
        });
        let ev =
            reader.read_response_event(crate::codec::test_host::HOST, "content_block_start", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_start".to_string(), data))
            );
        }

        let text_data = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello"}
        });
        let ev = reader.read_response_event(
            crate::codec::test_host::HOST,
            "content_block_delta",
            &text_data,
        );
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_delta".to_string(), text_data))
            );
        }

        let thinking_data = serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "thinking_delta", "thinking": "I need to think"}
        });
        let ev = reader.read_response_event(
            crate::codec::test_host::HOST,
            "content_block_delta",
            &thinking_data,
        );
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_delta".to_string(), thinking_data))
            );
        }

        let json_data = serde_json::json!({
            "type": "content_block_delta",
            "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": "{\"loc"}
        });
        let ev = reader.read_response_event(
            crate::codec::test_host::HOST,
            "content_block_delta",
            &json_data,
        );
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_delta".to_string(), json_data))
            );
        }

        let sig_data = serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "signature_delta", "signature": "sig_thinking_xyz"}
        });
        let ev = reader.read_response_event(
            crate::codec::test_host::HOST,
            "content_block_delta",
            &sig_data,
        );
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_delta".to_string(), sig_data))
            );
        }

        let data = serde_json::json!({"type": "content_block_stop", "index": 0});
        let ev =
            reader.read_response_event(crate::codec::test_host::HOST, "content_block_stop", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("content_block_stop".to_string(), data))
            );
        }

        let data = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "container": null,
                "stop_details": null,
                "stop_reason": "end_turn",
                "stop_sequence": null
            },
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 15,
                "output_tokens_details": null,
                "server_tool_use": null
            }
        });
        let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_delta", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            if let crate::codec::ir::IrStreamEvent::MessageDelta { stop_sequence, .. } = &e {
                assert_eq!(*stop_sequence, None);
            } else {
                panic!("expected MessageDelta");
            }
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("message_delta".to_string(), data))
            );
        }

        let data = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "container": null,
                "stop_details": null,
                "stop_reason": "stop_sequence",
                "stop_sequence": "\n\nHuman:"
            },
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 15,
                "output_tokens_details": null,
                "server_tool_use": null
            }
        });
        let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_delta", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            if let crate::codec::ir::IrStreamEvent::MessageDelta { stop_sequence, .. } = &e {
                assert_eq!(stop_sequence.as_deref(), Some("\n\nHuman:"));
            } else {
                panic!("expected MessageDelta");
            }
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("message_delta".to_string(), data))
            );
        }

        let data = serde_json::json!({"type": "message_stop"});
        let ev = reader.read_response_event(crate::codec::test_host::HOST, "message_stop", &data);
        assert!(ev.is_some());
        if let Some(e) = ev {
            assert_eq!(
                writer.write_response_event(crate::codec::test_host::HOST, &e),
                Some(("message_stop".to_string(), data))
            );
        }

        let data = serde_json::json!({
            "error": {"type": "invalid_request_error"}
        });
        let ev = reader.read_response_event(crate::codec::test_host::HOST, "error", &data);
        assert!(ev.is_some());
    }

    #[test]
    fn test_split_usage_decode_all_fields_distinct() {
        let reader = AnthropicReader;

        let data = serde_json::json!({
            "delta": {"stop_reason": "end_turn"},
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 30,
                "cache_read_input_tokens": 200
            }
        });

        let ev = reader
            .read_response_event(crate::codec::test_host::HOST, "message_delta", &data)
            .expect("should parse message_delta");

        if let crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: _,
            usage,
            ..
        } = ev
        {
            assert_eq!(usage.input_tokens, 100);
            assert_eq!(usage.output_tokens, 50);
            assert_eq!(usage.cache_creation_input_tokens, Some(30));
            assert_eq!(usage.cache_read_input_tokens, Some(200));

            assert_ne!(
                usage.input_tokens,
                usage.cache_creation_input_tokens.unwrap_or(0)
                    + usage.cache_read_input_tokens.unwrap_or(0),
                "input_tokens must not be collapsed into the cache token sum"
            );
        } else {
            panic!("expected MessageDelta event");
        }
    }

    #[test]
    fn test_signature_delta_verbatim_roundtrip() {
        let reader = AnthropicReader;
        let writer = AnthropicWriter;

        let sig = "sig_thinking_abc123xyz";
        let data = serde_json::json!({
            "index": 0,
            "delta": {
                "type": "signature_delta",
                "signature": sig
            }
        });

        let ev = reader
            .read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data)
            .expect("should parse");

        if let crate::codec::ir::IrStreamEvent::BlockDelta { index: _, delta } = ev {
            if let crate::codec::ir::IrDelta::SignatureDelta(s) = delta {
                assert_eq!(s, sig);
            } else {
                panic!("expected SignatureDelta variant");
            }
        } else {
            panic!("expected BlockDelta event");
        }

        let roundtrip = writer.write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::SignatureDelta(sig.to_string()),
            },
        );
        assert!(roundtrip.is_some());
        let (_, rt_data) = roundtrip.unwrap();

        let rt_sig = rt_data
            .get("delta")
            .and_then(|d| d.get("signature"))
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(rt_sig, sig);
    }

    #[test]
    fn test_openai_write_response_event_text_delta() {
        let writer = OpenAiWriter;
        let ev = crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hello".to_string()),
        };
        let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
        assert!(result.is_some());
        let (_, chunk) = result.unwrap();
        assert_eq!(
            chunk.get("object").and_then(|v| v.as_str()),
            Some("chat.completion.chunk")
        );
        let choices = chunk.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(choices.len(), 1);
        let choice = &choices[0];
        assert_eq!(choice.get("index").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(
            choice
                .get("delta")
                .and_then(|d| d.get("content").and_then(|c| c.as_str())),
            Some("hello")
        );
    }

    #[test]
    fn test_openai_write_response_event_message_start() {
        let writer = OpenAiWriter;
        let ev = crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        };
        let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
        assert!(result.is_some());
        let (_, chunk) = result.unwrap();
        assert_eq!(
            chunk.get("object").and_then(|v| v.as_str()),
            Some("chat.completion.chunk")
        );
        let choices = chunk.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(choices.len(), 1);
        let choice = &choices[0];
        assert_eq!(
            choice
                .get("delta")
                .and_then(|d| d.get("role").and_then(|r| r.as_str())),
            Some("assistant")
        );
    }

    #[test]
    fn test_openai_write_response_event_finish_reason_mapping() {
        let writer = OpenAiWriter;

        let ev1 = crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        };
        let result1 = writer.write_response_event(crate::codec::test_host::HOST, &ev1);
        assert!(result1.is_some());
        let (_, chunk1) = result1.unwrap();
        let choices1 = chunk1.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(
            choices1[0].get("finish_reason").and_then(|v| v.as_str()),
            Some("stop")
        );

        let ev2 = crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::MaxTokens),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        };
        let result2 = writer.write_response_event(crate::codec::test_host::HOST, &ev2);
        assert!(result2.is_some());
        let (_, chunk2) = result2.unwrap();
        let choices2 = chunk2.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(
            choices2[0].get("finish_reason").and_then(|v| v.as_str()),
            Some("length")
        );

        let ev3 = crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        };
        let result3 = writer.write_response_event(crate::codec::test_host::HOST, &ev3);
        assert!(result3.is_some());
        let (_, chunk3) = result3.unwrap();
        let choices3 = chunk3.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(
            choices3[0].get("finish_reason").and_then(|v| v.as_str()),
            Some("tool_calls")
        );
    }

    #[test]
    fn test_openai_write_response_event_tool_call_args() {
        let writer = OpenAiWriter;
        let ev = crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta(r#"{"x":1}"#.to_string()),
        };
        let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
        assert!(result.is_some());
        let (_, chunk) = result.unwrap();
        let choices = chunk.get("choices").and_then(|c| c.as_array()).unwrap();
        assert_eq!(choices.len(), 1);
        let choice = &choices[0];
        let tool_calls = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(|tc| tc.as_array())
            .unwrap();
        assert_eq!(tool_calls.len(), 1);
        let func_args = tool_calls[0]
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap();
        assert_eq!(func_args, r#"{"x":1}"#);
    }

    #[test]
    fn test_openai_write_response_event_lossy_drops() {
        let writer = OpenAiWriter;

        let ev1 = crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::ThinkingDelta("thinking...".to_string()),
        };
        assert!(writer
            .write_response_event(crate::codec::test_host::HOST, &ev1)
            .is_none());

        let ev2 = crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::SignatureDelta("sig...".to_string()),
        };
        assert!(writer
            .write_response_event(crate::codec::test_host::HOST, &ev2)
            .is_none());

        let ev3 = crate::codec::ir::IrStreamEvent::BlockStop { index: 0 };
        assert!(writer
            .write_response_event(crate::codec::test_host::HOST, &ev3)
            .is_none());

        let ev4 = crate::codec::ir::IrStreamEvent::MessageStop;
        assert!(writer
            .write_response_event(crate::codec::test_host::HOST, &ev4)
            .is_none());
    }

    #[test]
    fn test_openai_write_response_event_error() {
        let writer = OpenAiWriter;
        let err = busbar_contract::protocol::IrError {
            class: busbar_contract::upstream::StatusClass::ClientError,
            provider_signal: Some("boom".to_string()),
            retry_after: None,
        };
        let ev = crate::codec::ir::IrStreamEvent::Error(err);
        let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
        assert!(result.is_some());
        let (_, chunk) = result.unwrap();
        assert_eq!(
            chunk
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str()),
            Some("boom")
        );
    }

    fn assert_stripped_ok(input: &str) -> String {
        let out = crate::codec::dialect::strip_top_level_usage_member(input)
            .unwrap_or_else(|| panic!("expected a byte-level strip for: {input}"));
        let parsed: serde_json::Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("stripped output invalid: {e}: {out}"));
        assert!(
            parsed.get("usage").is_none(),
            "top-level usage must be gone: {out}"
        );
        out
    }

    #[test]
    fn strip_usage_last_key() {
        let input = r#"{"id":"x","choices":[{"delta":{"content":"hi"}}],"usage":null}"#;
        let out = assert_stripped_ok(input);
        assert_eq!(out, r#"{"id":"x","choices":[{"delta":{"content":"hi"}}]}"#);
    }

    #[test]
    fn strip_usage_first_key() {
        let input = r#"{"usage":null,"id":"x","choices":[]}"#;
        let out = assert_stripped_ok(input);
        assert_eq!(out, r#"{"id":"x","choices":[]}"#);
    }

    #[test]
    fn strip_usage_middle_key() {
        let input = r#"{"id":"x","usage":null,"choices":[{"delta":{}}]}"#;
        let out = assert_stripped_ok(input);
        assert_eq!(out, r#"{"id":"x","choices":[{"delta":{}}]}"#);
    }

    #[test]
    fn strip_usage_whitespace_variants() {
        let input = "{ \"id\" : \"x\" , \"usage\" : null , \"model\":\"m\" }";
        let out = assert_stripped_ok(input);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.get("id").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(parsed.get("model").and_then(|v| v.as_str()), Some("m"));
    }

    #[test]
    fn strip_usage_object_value() {
        let input =
            r#"{"id":"x","choices":[{"delta":{}}],"usage":{"prompt_tokens":10,"total_tokens":25}}"#;
        let out = assert_stripped_ok(input);
        assert_eq!(out, r#"{"id":"x","choices":[{"delta":{}}]}"#);
    }

    #[test]
    fn strip_usage_only_member() {
        let input = r#"{"usage":null}"#;
        let out = assert_stripped_ok(input);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.as_object().unwrap().is_empty());
    }

    #[test]
    fn strip_usage_never_corrupts_string_content() {
        let input =
            r#"{"id":"x","choices":[{"delta":{"content":"the json \"usage\":null tell"}}]}"#;
        assert!(
            crate::codec::dialect::strip_top_level_usage_member(input).is_none(),
            "must not match `usage` inside a string value"
        );
    }

    #[test]
    fn strip_usage_string_content_plus_real_key() {
        let input = r#"{"choices":[{"delta":{"content":"see \"usage\":null here"}}],"usage":null}"#;
        let out = assert_stripped_ok(input);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["choices"][0]["delta"]["content"].as_str(),
            Some("see \"usage\":null here"),
            "string content must survive the strip byte-for-byte"
        );
    }

    #[test]
    fn strip_usage_nested_usage_key_not_matched() {
        let input = r#"{"id":"x","meta":{"usage":null},"choices":[]}"#;
        assert!(
            crate::codec::dialect::strip_top_level_usage_member(input).is_none(),
            "nested usage must not be treated as the top-level member"
        );
    }

    #[test]
    fn strip_usage_no_usage_key_returns_none() {
        let input = r#"{"id":"x","choices":[{"delta":{"content":"hi"}}]}"#;
        assert!(crate::codec::dialect::strip_top_level_usage_member(input).is_none());
    }
}
