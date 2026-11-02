use super::*;
use crate::codec::ir::{IrBlockMeta, IrDelta, IrRole, IrStreamEvent};

#[test]
fn test_gemini_read_response_decode() {
    let j = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"text": "The weather in San Francisco is sunny."},
                    {"functionCall": {"name": "get_weather", "args": {"location": "San Francisco"}}}
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 8
        }
    });

    let reader = GeminiReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("should parse");

    assert_eq!(resp.content.len(), 2);

    if let crate::codec::ir::IrBlock::Text { text, .. } = &resp.content[0] {
        assert_eq!(text, "The weather in San Francisco is sunny.");
    } else {
        panic!("expected Text block");
    }

    if let crate::codec::ir::IrBlock::ToolUse {
        id: _, name, input, ..
    } = &resp.content[1]
    {
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
    } else {
        panic!("expected ToolUse block");
    }

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );

    assert_eq!(resp.usage.input_tokens, 15);
    assert_eq!(resp.usage.output_tokens, 8);
}

#[test]
fn test_gemini_read_write_response_roundtrip() {
    let j = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Hello, world!"}]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 3
        }
    });

    let reader = GeminiReader;
    let writer = GeminiWriter;

    let ir = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("should parse");
    let roundtrip = writer.write_response(crate::codec::test_host::HOST, &ir);

    assert_eq!(roundtrip, j, "whole-response round-trip must be identical");
}

#[test]
fn test_gemini_same_protocol_minimal_response_omits_synthesized_identity() {
    let j = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hi"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 3
        }
    });

    let reader = GeminiReader;
    let writer = GeminiWriter;
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &j)
        .expect("should parse");

    assert_eq!(ir.id, None, "no responseId in the minimal native body");
    assert_eq!(ir.created, None, "Gemini bodies carry no timestamp");
    assert_eq!(ir.model, None, "no modelVersion in the minimal body");

    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert!(
        out.get("responseId").is_none(),
        "minimal same-protocol roundtrip must NOT synthesize a responseId: {out}"
    );
    assert!(
        out["usageMetadata"].get("totalTokenCount").is_none(),
        "minimal same-protocol roundtrip must NOT inject totalTokenCount: {out}"
    );

    assert_eq!(
        out, j,
        "Gemini→Gemini minimal response roundtrip must remain byte-identical"
    );
}

#[test]
fn test_singular_read_response_event_delegates_for_fanout_protocols() {
    let reader = GeminiReader;
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello"}]},
            "finishReason": null
        }]
    });

    let singular = reader.read_response_event(crate::codec::test_host::HOST, "", &chunk);
    let mut st = crate::codec::ir::StreamDecodeState::default();
    let plural_first = reader
        .read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut st)
        .into_iter()
        .next();
    assert!(
        singular.is_some(),
        "default singular read_response_event must not be a dead None stub"
    );
    assert_eq!(
        singular, plural_first,
        "default singular must equal the fan-out's first event"
    );

    let empty = serde_json::json!({});
    let mut st2 = crate::codec::ir::StreamDecodeState::default();
    assert_eq!(
        GeminiReader.read_response_event(crate::codec::test_host::HOST, "", &empty),
        GeminiReader
            .read_response_events(crate::codec::test_host::HOST, "", &empty, &mut st2)
            .into_iter()
            .next(),
        "default singular must track the fan-out's first event on any input"
    );
}

#[test]
fn test_gemini_read_response_events_stream_fanout() {
    let reader = GeminiReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let chunk1 = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello"}]},
            "finishReason": null
        }]
    });

    let chunk2 = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": ", world!"}]},
            "finishReason": null
        }]
    });

    let chunk3 = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": []},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 5
        }
    });

    let mut events: Vec<IrStreamEvent> = Vec::new();

    for chunk in [chunk1.clone(), chunk2.clone(), chunk3.clone()] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut state,
        ));
    }

    assert_eq!(events.len(), 7);

    assert!(matches!(
        events[0],
        IrStreamEvent::MessageStart {
            role: IrRole::Assistant,
            usage: None,
            ..
        }
    ));

    assert!(matches!(
        events[1],
        IrStreamEvent::BlockStart {
            index: 0,
            block: IrBlockMeta::Text,
            refusal: _,
        }
    ));

    if let IrStreamEvent::BlockDelta { index: idx, delta } = &events[2] {
        assert_eq!(*idx, 0);
        if let IrDelta::TextDelta(text) = delta {
            assert_eq!(text, "Hello");
        } else {
            panic!("expected TextDelta");
        }
    } else {
        panic!("expected BlockDelta");
    }

    if let IrStreamEvent::BlockDelta { index: idx, delta } = &events[3] {
        assert_eq!(*idx, 0);
        if let IrDelta::TextDelta(text) = delta {
            assert_eq!(text, ", world!");
        } else {
            panic!("expected TextDelta");
        }
    } else {
        panic!("expected BlockDelta");
    }

    assert!(matches!(events[4], IrStreamEvent::BlockStop { index: 0 }));

    if let IrStreamEvent::MessageDelta {
        stop_reason, usage, ..
    } = &events[5]
    {
        assert_eq!(stop_reason, &Some(crate::codec::ir::IrStopReason::EndTurn));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
    } else {
        panic!("expected MessageDelta");
    }

    assert!(matches!(events[6], IrStreamEvent::MessageStop));
}

#[test]
fn test_gemini_write_response_event_text_delta() {
    let writer = GeminiWriter;

    let ev = IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
    };

    let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
    assert!(result.is_some());

    let (_, chunk) = result.unwrap();

    let candidates = chunk.get("candidates").and_then(|c| c.as_array()).unwrap();
    assert_eq!(candidates.len(), 1);

    let candidate = &candidates[0];
    let content = candidate.get("content").unwrap();

    assert_eq!(content.get("role").and_then(|r| r.as_str()), Some("model"));

    let parts_arr = content.get("parts").and_then(|p| p.as_array()).unwrap();
    assert_eq!(parts_arr.len(), 1);

    let part = &parts_arr[0];
    assert_eq!(part.get("text").and_then(|t| t.as_str()), Some("hi"));
}

#[test]
fn test_gemini_write_response_event_message_delta() {
    let writer = GeminiWriter;

    let ev = IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };

    let result = writer.write_response_event(crate::codec::test_host::HOST, &ev);
    assert!(result.is_some());

    let (_, chunk) = result.unwrap();

    let candidates = chunk.get("candidates").and_then(|c| c.as_array()).unwrap();
    assert_eq!(candidates.len(), 1);

    let candidate = &candidates[0];
    assert_eq!(
        candidate.get("finishReason").and_then(|r| r.as_str()),
        Some("STOP")
    );

    let usage = chunk.get("usageMetadata").expect("usageMetadata emitted");
    assert_eq!(
        usage.get("promptTokenCount").and_then(|v| v.as_u64()),
        Some(10),
        "promptTokenCount must carry the event's input_tokens"
    );
    assert_eq!(
        usage.get("candidatesTokenCount").and_then(|v| v.as_u64()),
        Some(5),
        "candidatesTokenCount must carry the event's output_tokens"
    );
}

#[test]
fn test_gemini_read_response_events_function_call() {
    let reader = GeminiReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let chunk1 = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Let me check"}]},
            "finishReason": null
        }]
    });

    let chunk2 = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"text": ""},
                    {"functionCall": {"name": "get_weather", "args": {"location": "SF"}}}
                ]
            },
            "finishReason": null
        }]
    });

    let chunk3 = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": []},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 20,
            "candidatesTokenCount": 10
        }
    });

    let mut events: Vec<IrStreamEvent> = Vec::new();

    for chunk in [chunk1.clone(), chunk2.clone(), chunk3.clone()] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut state,
        ));
    }

    assert!(events.len() >= 6);

    let mut found_tool_block_start = false;
    let mut found_tool_args_delta = false;

    for event in &events {
        match event {
            IrStreamEvent::BlockStart {
                index: _,
                block: crate::codec::ir::IrBlockMeta::ToolUse { id: _, name },
                ..
            } => {
                if *name == "get_weather" {
                    found_tool_block_start = true;
                }
            }

            IrStreamEvent::BlockDelta {
                delta: IrDelta::InputJsonDelta(json_str),
                ..
            } => {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if args.get("location").is_some() {
                        found_tool_args_delta = true;
                    }
                }
            }
            _ => {}
        }
    }

    assert!(found_tool_block_start, "should have ToolUse BlockStart");
    assert!(
        found_tool_args_delta,
        "should have InputJsonDelta with args"
    );
}

/// Helper: split a concatenated OpenAI SSE byte stream into its per-frame JSON chunk objects
fn openai_sse_chunks(bytes: &[u8]) -> Vec<serde_json::Value> {
    let text = std::str::from_utf8(bytes).expect("openai SSE is utf-8");
    let mut chunks = Vec::new();
    for frame in text.split("\n\n") {
        let Some(rest) = frame.lines().find_map(|l| l.strip_prefix("data:")) else {
            continue;
        };
        let payload = rest.strip_prefix(' ').unwrap_or(rest).trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            chunks.push(v);
        }
    }
    chunks
}

/// OpenAI per-chunk identity: the real OpenAI API repeats the top-level
#[test]
fn test_openai_ingress_per_chunk_identity_repeated() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("openai ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_backend\",\"role\":\"assistant\",\"model\":\"claude-x\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(t.finish(crate::codec::test_host::HOST));

    let chunks = openai_sse_chunks(&raw);
    assert!(
        chunks.len() >= 2,
        "expected multiple chunks; got {}",
        chunks.len()
    );

    let first_id = chunks[0]
        .get("id")
        .and_then(|v| v.as_str())
        .expect("first chunk has an id")
        .to_string();

    assert!(
        first_id.starts_with("chatcmpl-"),
        "cross-protocol id must be a native chatcmpl- id; got {first_id}"
    );
    let first_created = chunks[0].get("created").and_then(|v| v.as_u64());
    assert!(first_created.is_some(), "first chunk has a created");
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(
            c.get("object").and_then(|v| v.as_str()),
            Some("chat.completion.chunk"),
            "chunk {i} object; got {c}"
        );
        assert_eq!(
            c.get("id").and_then(|v| v.as_str()),
            Some(first_id.as_str()),
            "chunk {i} must repeat the SAME stream id; got {c}"
        );
        assert_eq!(
            c.get("created").and_then(|v| v.as_u64()),
            first_created,
            "chunk {i} must repeat the SAME created; got {c}"
        );
        assert_eq!(
            c.get("model").and_then(|v| v.as_str()),
            Some("claude-x"),
            "chunk {i} must repeat the stream model; got {c}"
        );
    }

    assert!(
        !raw.windows(b"msg_backend".len())
            .any(|w| w == b"msg_backend"),
        "foreign backend id must be stripped on cross-protocol ingress"
    );
}

/// The OpenAI ingress chunk-identity replay (behind the `StreamFraming`
#[test]
fn test_openai_ingress_mid_stream_error_envelope_unchanged_by_identity() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("openai ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [

            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\",\"model\":\"claude-x\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",

            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream overloaded\"}}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(t.finish(crate::codec::test_host::HOST));

    let chunks = openai_sse_chunks(&raw);

    let error_frame = chunks
        .iter()
        .find(|c| c.get("error").is_some())
        .expect("a mid-stream error envelope must be emitted to the OpenAI client");

    assert!(
            error_frame.get("id").is_none(),
            "error envelope must NOT receive an injected `id` (would corrupt the shape); got {error_frame}"
        );
    assert!(
        error_frame.get("created").is_none(),
        "error envelope must NOT receive an injected `created`; got {error_frame}"
    );
    assert!(
        error_frame.get("object").is_none(),
        "error envelope is not a chat.completion.chunk and carries no `object`; got {error_frame}"
    );

    assert!(
        error_frame
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(|v| v.as_str())
            .is_some(),
        "error envelope must carry an `error.type`; got {error_frame}"
    );

    let had_identity_chunk = chunks.iter().any(|c| {
        c.get("object").and_then(|v| v.as_str()) == Some("chat.completion.chunk")
            && c.get("id").is_some()
    });
    assert!(
        had_identity_chunk,
        "real chat.completion.chunk frames must still carry the latched id (guard is selective)"
    );
}

/// Bedrock messageStop+metadata fan-out (real latencyMs): a bedrock->bedrock stream must
#[test]
fn test_bedrock_to_bedrock_stream_roundtrips_stop_and_metadata() {
    assert!(
        StreamTranslate::new("bedrock", "bedrock").is_none(),
        "bedrock->bedrock needs no translator (native passthrough)"
    );

    let mut t = StreamTranslate::new("bedrock", "openai").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(t.finish(crate::codec::test_host::HOST));

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all frames decode cleanly; {} left",
        buf.len()
    );
    let types: Vec<&str> = frames.iter().map(|(et, _)| et.as_str()).collect();

    let stop_pos = types
        .iter()
        .position(|t| *t == "messageStop")
        .expect("messageStop frame present");
    let meta_pos = types
        .iter()
        .position(|t| *t == "metadata")
        .expect("metadata frame present");
    assert!(
        stop_pos < meta_pos,
        "messageStop must precede metadata (native order); got {types:?}"
    );

    let meta = frames
        .iter()
        .find(|(et, _)| et == "metadata")
        .expect("metadata frame");
    let mv: serde_json::Value = serde_json::from_slice(&meta.1).expect("valid metadata JSON");
    assert_eq!(
        mv.pointer("/usage/inputTokens").and_then(|x| x.as_u64()),
        Some(7),
        "usage inputTokens; got {mv}"
    );
    assert_eq!(
        mv.pointer("/usage/outputTokens").and_then(|x| x.as_u64()),
        Some(3),
        "usage outputTokens; got {mv}"
    );
    assert!(
        mv.pointer("/metrics/latencyMs")
            .and_then(|x| x.as_u64())
            .is_some(),
        "metadata must carry a real metrics.latencyMs; got {mv}"
    );

    let stop = frames
        .iter()
        .find(|(et, _)| et == "messageStop")
        .expect("messageStop frame");
    let sv: serde_json::Value = serde_json::from_slice(&stop.1).expect("valid messageStop JSON");
    assert_eq!(
        sv.get("stopReason").and_then(|x| x.as_str()),
        Some("end_turn"),
        "stop reason maps to end_turn; got {sv}"
    );
}

/// On a BEDROCK-ingress cross-protocol stream the translator's OUTPUT is binary
#[test]
fn test_bedrock_ingress_ir_usage_carries_real_tokens() {
    let mut t = StreamTranslate::new("bedrock", "openai").expect("bedrock ingress translator");
    let mut binary_out: Vec<u8> = Vec::new();
    for frame in [
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n",
        ] {
            binary_out.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    binary_out.extend(t.finish(crate::codec::test_host::HOST));

    let usage = t.usage().expect("A-tap captured terminal usage");
    assert_eq!(
        usage.input_tokens, 11,
        "A-tap reads input tokens from the IR"
    );
    assert_eq!(
        usage.output_tokens, 4,
        "A-tap reads output tokens from the IR"
    );

    assert!(!binary_out.is_empty(), "binary frames were emitted");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&binary_out).is_err(),
        "translator output is binary eventstream framing, not a JSON document"
    );

    let mut buf = binary_out.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        frames.iter().any(|(et, _)| et == "metadata"),
        "binary output contains the eventstream metadata frame"
    );
}

/// A Gemini `cachedContent` reference is dropped on the cross-protocol seam (no
#[test]
fn gemini_cached_content_warns_naming_truncation_and_billing() {
    use busbar_test_kit::warn_capture::WarnCapture;

    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "cachedContent": "projects/p/locations/l/cachedContents/c"
    });
    let ir = GeminiReader.read_request(&body).expect("parses");
    assert_eq!(
        ir.extra.get("cachedContent").and_then(|v| v.as_str()),
        Some("projects/p/locations/l/cachedContents/c"),
        "cachedContent must ride extra (unmodeled key)"
    );

    let prep = crate::codec::egress_prep::EgressPrep {
        thought_signature_fill: false,
        ingress_protocol: "gemini",
        egress_requires_max_tokens: false,
        lane_default_max_tokens: None,
        global_default_max_tokens: 4096,
        reasoning_allowed: true,
        reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
        prompt_caching_allowed: true,
        cache_control_cap: None,
        lane_caps: Default::default(),
    };

    let seam = crate::codec::drops::Seam {
        direction: crate::codec::drops::Direction::Request,
        ingress: "gemini",
        egress: "openai",
    };
    let cap = WarnCapture::default();
    let subscriber = cap.clone();
    let mut req = ir;
    let ((), dropped) = tracing::subscriber::with_default(subscriber, || {
        crate::codec::drops::scope(seam, || {
            crate::codec::chat_handle::chat_prepare_for_egress(&mut req, &prep)
        })
    });
    assert_eq!(
        dropped,
        vec!["cachedContent".to_string()],
        "the seam's audit names the dropped path once"
    );

    assert!(
        cap.contains("VISIBLE history") && cap.contains("UNCACHED"),
        "the diagnostic must name BOTH consequences (truncated history AND full billing): {:?}",
        cap.messages()
    );

    let body2 = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let ir2 = GeminiReader.read_request(&body2).expect("parses");
    let cap2 = WarnCapture::capturing_debug();
    let subscriber2 = cap2.clone();
    let mut req2 = ir2;
    tracing::subscriber::with_default(subscriber2, || {
        crate::codec::drops::scope(seam, || {
            crate::codec::chat_handle::chat_prepare_for_egress(&mut req2, &prep)
        })
    });
    assert!(
        !cap2.contains("cachedContent"),
        "a request without cachedContent must not warn about dropping it: {:?}",
        cap2.messages()
    );
}

/// Google's `CitationSource.startIndex`/`endIndex` are documented measured in
#[test]
fn gemini_citation_byte_indices_convert_to_characters() {
    let text = "héllo wörld";
    let byte_start = text.find("wörld").unwrap() as i64;
    let byte_end = byte_start + "wörld".len() as i64;

    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": "STOP",
            "citationMetadata": {
                "citationSources": [{
                    "startIndex": byte_start,
                    "endIndex": byte_end,
                    "uri": "https://example.com"
                }]
            }
        }]
    });
    let ir = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("parses");
    let citations = ir
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations),
            _ => None,
        })
        .expect("text block with citations");
    assert_eq!(citations.len(), 1, "{citations:?}");
    assert_eq!(
        citations[0].start_index,
        Some(6),
        "byte offset 7 must convert to CHAR offset 6: {citations:?}"
    );
    assert_eq!(
        citations[0].end_index,
        Some(11),
        "byte offset 13 must convert to CHAR offset 11: {citations:?}"
    );
}

/// Writer inverse: `write_gemini_citation`
#[test]
fn gemini_writer_reemits_char_indices_as_bytes_for_foreign_citations() {
    let text = "héllo wörld";
    let citation = crate::codec::ir::IrCitation {
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
    };
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::Text {
            text: text.to_string(),
            cache_control: None,
            citations: vec![citation],
            refusal: false,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gemini-1.5-pro".into()),
        id: None,
        created: None,
        system_fingerprint: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let gemini_writer = GeminiWriter;
    let out = gemini_writer.write_response(crate::codec::test_host::HOST, &resp);
    let src = &out["candidates"][0]["citationMetadata"]["citationSources"][0];
    assert_eq!(
        src["startIndex"], 7,
        "CHAR offset 6 must convert back to BYTE offset 7: {src:?}"
    );
    assert_eq!(
        src["endIndex"], 13,
        "CHAR offset 11 must convert back to BYTE offset 13: {src:?}"
    );
}
