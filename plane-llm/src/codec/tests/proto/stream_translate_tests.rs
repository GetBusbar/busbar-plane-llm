use super::*;
use crate::codec::ir::{IrBlockMeta, IrStreamEvent};
use busbar_contract::http::StatusCode;

/// The canonical IR text stream: start → open text block → `text` delta → close → terminal delta
fn canonical_text_events(text: &str) -> [IrStreamEvent; 6] {
    [
        crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: Some("test-model".to_string()),
        },
        crate::codec::ir::IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta(text.to_string()),
        },
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
        crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage {
                input_tokens: 7,
                output_tokens: 3,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                detail: crate::codec::ir::IrUsageDetail::default(),
            },
            stop_detail: None,
        },
        crate::codec::ir::IrStreamEvent::MessageStop,
    ]
}

/// STRUCTURAL ROUND-TRIP MATRIX (the test class that would have caught the cohere object-shape
#[test]
fn test_all_protocols_stream_write_read_roundtrip_preserves_text_and_terminal() {
    let text = "Hello, world";

    let events = canonical_text_events(text);

    for proto in [
        Protocol::anthropic(),
        Protocol::openai(),
        Protocol::gemini(),
        Protocol::bedrock(),
        Protocol::responses(),
        Protocol::cohere(),
    ] {
        let name = proto.name_static().to_string();

        let frames: Vec<(String, serde_json::Value)> = events
            .iter()
            .filter_map(|ev| {
                proto
                    .writer()
                    .write_response_event(crate::codec::test_host::HOST, ev)
            })
            .collect();
        assert!(
            !frames.is_empty(),
            "{name}: writer produced no frames for a normal text stream"
        );

        let mut state = crate::codec::ir::StreamDecodeState::default();
        let mut decoded: Vec<crate::codec::ir::IrStreamEvent> = Vec::new();
        for (et, data) in &frames {
            let mut data = data.clone();
            if !et.is_empty() {
                if let Some(obj) = data.as_object_mut() {
                    obj.entry("type")
                        .or_insert_with(|| serde_json::Value::String(et.clone()));
                }
            }
            decoded.extend(proto.reader().read_response_events(
                crate::codec::test_host::HOST,
                et,
                &data,
                &mut state,
            ));
        }

        let got: String = decoded
            .iter()
            .filter_map(|e| match e {
                crate::codec::ir::IrStreamEvent::BlockDelta {
                    delta: crate::codec::ir::IrDelta::TextDelta(t),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got, text,
            "{name}: streamed text must round-trip writer→reader; got {got:?} from {decoded:?}"
        );

        if name != "bedrock" {
            let has_terminal = decoded.iter().any(|e| {
                matches!(e, crate::codec::ir::IrStreamEvent::MessageStop)
                    || matches!(
                        e,
                        crate::codec::ir::IrStreamEvent::MessageDelta {
                            stop_reason: Some(_),
                            ..
                        }
                    )
            });
            assert!(
                    has_terminal,
                    "{name}: a terminal (MessageStop / stop_reason MessageDelta) must round-trip; got {decoded:?}"
                );
        }
    }
}

/// Encode one IR-derived `(event_type, json)` frame as wire bytes for the given protocol's
fn encode_wire_frame(
    is_eventstream: bool,
    event_type: &str,
    data: &serde_json::Value,
    out: &mut Vec<u8>,
) {
    if is_eventstream {
        let payload = serde_json::to_vec(data).unwrap_or_default();
        out.extend_from_slice(&crate::codec::eventstream::encode_frame(
            event_type, &payload,
        ));
    } else {
        write_sse_frame(out, event_type, data);
    }
}

/// Serialize a canonical IR event stream through `proto`'s OWN writer into realistic native wire
/// bytes for that protocol — used to build EGRESS-shaped input for `StreamTranslate::feed`. Routes
/// every emitted chunk through `proto`'s OWN `StreamFraming` seams — the SAME vtable
/// `StreamTranslate` itself drives when `proto` is the INGRESS — so the fixture reproduces each
/// protocol's genuine multi-frame wire quirks instead of the single-frame shape the 1:1
/// `write_response_event` trait can express on its own:
///   - `on_combined_stop_delta`: fans a combined terminal `MessageDelta{stop_reason: Some, usage}`
///     into Bedrock's native two-frame `messageStop`+`metadata` split. Without it, feeding a
///     Bedrock-egress translator only a lone `messageStop` (no `metadata`) would never satisfy
///     `BedrockReader`'s deferred-emit-on-metadata contract and the terminal event would never
///     surface — a self-inflicted false negative, not a real protocol gap.
///   - `on_egress_chunk` (with `client_include_usage(true)`, matching busbar's own upstream
///     behavior of always forcing `include_usage` to bill): un-folds OpenAI's writer-folded
///     terminal usage back into the genuine SEPARATE trailing `{choices:[], usage:{...}}` chunk a
///     real OpenAI-compatible upstream sends. Without this the fixture only ever exercises
///     `StreamTranslate`'s SINGLE-message_delta terminal path, never the trailing-usage-only
///     `MessageDelta` merge branch (`on_usage_only_delta` / `folds_terminal_usage`'s
///     `merge_trailing_usage`) that a real OpenAI-egress stream drives on every request. That gap
///     is closed here for OpenAI, the only protocol besides Bedrock whose `new_stream_framing()`
///     isn't the inert `PassthroughFraming`.
///
/// For every protocol where both seams are no-ops (`PassthroughFraming`), the event is written
/// as-is, unchanged from the diagonal test's plain `write_response_event` loop.
fn emit_via_framing(
    framing: &mut dyn StreamFraming,
    is_es: bool,
    et: &str,
    mut data: serde_json::Value,
    out: &mut Vec<u8>,
) {
    if let Some(trailing) = framing.on_egress_chunk(&mut data) {
        encode_wire_frame(is_es, et, &data, out);
        encode_wire_frame(is_es, et, &trailing, out);
    } else {
        encode_wire_frame(is_es, et, &data, out);
    }
}

fn encode_ir_events_as_wire(
    proto: &Protocol,
    events: &[crate::codec::ir::IrStreamEvent],
) -> Vec<u8> {
    let is_es = proto.decl().is_some_and(|d| d.ingress_is_eventstream);
    let mut framing = proto.writer().new_stream_framing();
    framing.set_client_include_usage(true);
    let mut out = Vec::new();
    for ev in events {
        if let crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(reason),
            usage,
            stop_sequence,
            stop_detail: _,
        } = ev
        {
            if let Some(sub_events) =
                framing.on_combined_stop_delta(*reason, stop_sequence.clone(), usage)
            {
                for sub in &sub_events {
                    if let Some((et, data)) = proto
                        .writer()
                        .write_response_event(crate::codec::test_host::HOST, sub)
                    {
                        emit_via_framing(framing.as_mut(), is_es, &et, data, &mut out);
                    }
                }
                continue;
            }
        }
        if let Some((et, data)) = proto
            .writer()
            .write_response_event(crate::codec::test_host::HOST, ev)
        {
            emit_via_framing(framing.as_mut(), is_es, &et, data, &mut out);
        }
    }
    if let Some(flush) = framing.on_finish() {
        if let Some((et, data)) = proto
            .writer()
            .write_response_event(crate::codec::test_host::HOST, &flush)
        {
            emit_via_framing(framing.as_mut(), is_es, &et, data, &mut out);
        }
    }
    out
}

/// Decode wire bytes (as produced by a protocol's own writer, OR by `StreamTranslate::feed`/
fn decode_wire_frames(is_eventstream: bool, bytes: &[u8]) -> Vec<(String, serde_json::Value)> {
    if is_eventstream {
        let mut buf = bytes.to_vec();
        crate::codec::eventstream::drain_frames(&mut buf)
            .into_iter()
            .filter_map(|(et, payload)| {
                serde_json::from_slice::<serde_json::Value>(&payload)
                    .ok()
                    .map(|d| (et, d))
            })
            .collect()
    } else {
        let mut frames = Vec::new();
        let mut consumed = 0usize;
        while let Some((rel, term_len)) = find_frame_terminator(&bytes[consumed..]) {
            let end = consumed + rel + term_len;
            let frame = &bytes[consumed..end];
            consumed = end;
            let Some((event_type, data_str)) = parse_sse_frame(frame) else {
                continue;
            };
            if data_str.is_empty() || data_str == SSE_DONE_SENTINEL {
                continue;
            }
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&data_str) {
                frames.push((event_type.to_string(), data));
            }
        }
        frames
    }
}

/// FULL CROSS-PRODUCT STRUCTURAL TRANSLATION MATRIX: for every DIRECTED (ingress, egress) pair
#[test]
fn test_all_protocol_pairs_stream_translate_roundtrip_preserves_text_and_terminal() {
    let text = "Hello, world";
    let events = canonical_text_events(text);

    let protocol_names = [
        "anthropic",
        "openai",
        "gemini",
        "bedrock",
        "responses",
        "cohere",
    ];

    let mut pairs_tested: usize = 0;
    for &ingress in &protocol_names {
        for &egress in &protocol_names {
            if ingress == egress {
                continue;
            }
            let ingress_proto = protocol_for(ingress).expect("known ingress protocol");
            let egress_proto = protocol_for(egress).expect("known egress protocol");
            let ingress_is_es = ingress_proto
                .decl()
                .is_some_and(|d| d.ingress_is_eventstream);

            let wire = encode_ir_events_as_wire(&egress_proto, &events);
            assert!(
                !wire.is_empty(),
                "{ingress}<-{egress}: egress writer produced no wire bytes for a normal text stream"
            );

            let mut st = StreamTranslate::new(ingress, egress)
                .unwrap_or_else(|| panic!("{ingress}<-{egress}: StreamTranslate::new returned None for a genuinely different pair"));
            let mut out = st.feed(crate::codec::test_host::HOST, &wire);
            out.extend_from_slice(&st.finish(crate::codec::test_host::HOST));
            assert!(
                !st.aborted(),
                "{ingress}<-{egress}: translator aborted on a normal text stream; out so far: {out:?}"
            );

            let mut state = crate::codec::ir::StreamDecodeState::default();
            let mut decoded: Vec<crate::codec::ir::IrStreamEvent> = Vec::new();
            for (et, data) in decode_wire_frames(ingress_is_es, &out) {
                let mut data = data;
                if !et.is_empty() {
                    if let Some(obj) = data.as_object_mut() {
                        obj.entry("type")
                            .or_insert_with(|| serde_json::Value::String(et.clone()));
                    }
                }
                decoded.extend(ingress_proto.reader().read_response_events(
                    crate::codec::test_host::HOST,
                    &et,
                    &data,
                    &mut state,
                ));
            }

            let got: String = decoded
                .iter()
                .filter_map(|e| match e {
                    crate::codec::ir::IrStreamEvent::BlockDelta {
                        delta: crate::codec::ir::IrDelta::TextDelta(t),
                        ..
                    } => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                got, text,
                "{ingress}<-{egress}: streamed text must survive cross-protocol translation; got {got:?} from {decoded:?}"
            );

            let mut open_indices = std::collections::BTreeSet::new();
            for ev in &decoded {
                match ev {
                    crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => {
                        open_indices.insert(*index);
                    }
                    crate::codec::ir::IrStreamEvent::BlockStop { index } => {
                        open_indices.remove(index);
                    }
                    crate::codec::ir::IrStreamEvent::BlockDelta {
                        index,
                        delta: crate::codec::ir::IrDelta::TextDelta(_),
                    } => {
                        assert!(
                            open_indices.contains(index),
                            "{ingress}<-{egress}: a text delta at index {index} arrived with no \
                             matching open BlockStart — index corruption in translation; got {decoded:?}"
                        );
                    }
                    _ => {}
                }
            }

            let has_terminal = decoded.iter().any(|e| {
                matches!(e, crate::codec::ir::IrStreamEvent::MessageStop)
                    || matches!(
                        e,
                        crate::codec::ir::IrStreamEvent::MessageDelta {
                            stop_reason: Some(_),
                            ..
                        }
                    )
            });
            assert!(
                has_terminal,
                "{ingress}<-{egress}: a terminal (MessageStop / stop_reason MessageDelta) must survive cross-protocol translation; got {decoded:?}"
            );

            pairs_tested += 1;
        }
    }
    assert_eq!(
        pairs_tested, 30,
        "must genuinely drive all 6*5 = 30 directed cross-protocol pairs, got {pairs_tested}"
    );
}

/// NON-STREAMING round-trip matrix (the class the responses `write_response` bug fell in): for
#[test]
fn test_all_protocols_nonstream_write_read_roundtrip_preserves_text() {
    let text = "Hello, world";
    for proto in [
        Protocol::anthropic(),
        Protocol::openai(),
        Protocol::gemini(),
        Protocol::bedrock(),
        Protocol::responses(),
        Protocol::cohere(),
    ] {
        let name = proto.name_static().to_string();
        let resp = crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::Text {
                text: text.to_string(),
                cache_control: None,
                citations: Vec::new(),
                refusal: false,
            }],
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage {
                input_tokens: 7,
                output_tokens: 3,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                detail: crate::codec::ir::IrUsageDetail::default(),
            },
            model: Some("test-model".to_string()),
            id: Some("resp_x".to_string()),
            created: Some(1_700_000_000),
            system_fingerprint: None,

            request_echo: None,
            stop_detail: None,
            ..Default::default()
        };
        let body = proto
            .writer()
            .write_response(crate::codec::test_host::HOST, &resp);
        let back = proto
            .reader()
            .read_response(crate::codec::test_host::HOST, &body)
            .unwrap_or_else(|e| {
                panic!("{name}: write_response output must read back: {e:?}\nbody: {body}")
            });
        let got: String = back
            .content
            .iter()
            .filter_map(|b| match b {
                crate::codec::ir::IrBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got, text,
            "{name}: non-streaming text must round-trip write_response→read_response; got {got:?}"
        );
    }
}

/// Build a minimal native assistant response body for `proto` carrying `usage` verbatim. Used to
fn native_response_with_usage(name: &str, usage: serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    match name {
        "anthropic" => json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn", "model": "claude", "usage": usage
        }),
        "openai" => json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": usage
        }),
        "gemini" => json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
            "modelVersion": "gemini-pro",
            "usageMetadata": usage
        }),
        "responses" => json!({
            "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-4o",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": usage
        }),
        "bedrock" => json!({
            "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
            "stopReason": "end_turn", "usage": usage
        }),
        "cohere" => json!({
            "id": "co_1", "finish_reason": "COMPLETE",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
            "usage": usage
        }),
        other => panic!("unknown protocol {other}"),
    }
}

/// (A) ROUND-TRIP FIDELITY with cache tokens: each protocol that carries cache fields, read a
#[test]
fn a2_roundtrip_usage_fidelity_with_cache() {
    use serde_json::json;

    {
        let p = protocol_for("openai").unwrap();
        let native = native_response_with_usage(
            "openai",
            json!({"prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 5}}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.input_tokens, 95, "openai: uncached input");
        assert_eq!(ir.usage.cache_read_input_tokens, Some(5));
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out["usage"]["prompt_tokens"],
            json!(100),
            "openai: prompt_total reconstructed"
        );
        assert_eq!(out["usage"]["completion_tokens"], json!(10));
        assert_eq!(
            out["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(5)
        );
    }

    {
        let p = protocol_for("gemini").unwrap();
        let native = native_response_with_usage(
            "gemini",
            json!({"promptTokenCount": 150, "candidatesTokenCount": 20, "cachedContentTokenCount": 30, "totalTokenCount": 170}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.input_tokens, 120, "gemini: uncached input");
        assert_eq!(ir.usage.cache_read_input_tokens, Some(30));
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out["usageMetadata"]["promptTokenCount"],
            json!(150),
            "gemini: prompt_total reconstructed"
        );
        assert_eq!(out["usageMetadata"]["candidatesTokenCount"], json!(20));
        assert_eq!(out["usageMetadata"]["cachedContentTokenCount"], json!(30));
        assert_eq!(out["usageMetadata"]["totalTokenCount"], json!(170));
    }

    {
        let p = protocol_for("responses").unwrap();
        let native = native_response_with_usage(
            "responses",
            json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 64}}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.input_tokens, 36, "responses: uncached input");
        assert_eq!(ir.usage.cache_read_input_tokens, Some(64));
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out["usage"]["input_tokens"],
            json!(100),
            "responses: input_total reconstructed"
        );
        assert_eq!(out["usage"]["output_tokens"], json!(10));
        assert_eq!(
            out["usage"]["input_tokens_details"]["cached_tokens"],
            json!(64)
        );
    }

    {
        let p = protocol_for("anthropic").unwrap();
        let native = native_response_with_usage(
            "anthropic",
            json!({"input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 5000, "cache_creation_input_tokens": 2000}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.input_tokens, 100,
            "anthropic: input stays uncached (additive)"
        );
        assert_eq!(ir.usage.cache_read_input_tokens, Some(5000));
        assert_eq!(ir.usage.cache_creation_input_tokens, Some(2000));
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out["usage"]["input_tokens"],
            json!(100),
            "anthropic: input re-emitted verbatim"
        );
        assert_eq!(out["usage"]["cache_read_input_tokens"], json!(5000));
        assert_eq!(out["usage"]["cache_creation_input_tokens"], json!(2000));
    }

    {
        let p = protocol_for("bedrock").unwrap();
        let native = native_response_with_usage(
            "bedrock",
            json!({"inputTokens": 100, "outputTokens": 50, "cacheReadInputTokens": 5000, "cacheWriteInputTokens": 2000}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.input_tokens, 100,
            "bedrock: input stays uncached (additive)"
        );
        assert_eq!(ir.usage.cache_read_input_tokens, Some(5000));
        assert_eq!(ir.usage.cache_creation_input_tokens, Some(2000));
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(
            out["usage"]["inputTokens"],
            json!(100),
            "bedrock: input re-emitted verbatim"
        );
        assert_eq!(out["usage"]["cacheReadInputTokens"], json!(5000));
        assert_eq!(out["usage"]["cacheWriteInputTokens"], json!(2000));
    }
}

/// (A) NO-CACHE fidelity: a response with NO cache fields must emit NO cache sub-object/field for
#[test]
fn a2_roundtrip_usage_fidelity_no_cache_emits_no_cache_object() {
    use serde_json::json;

    {
        let p = protocol_for("openai").unwrap();
        let native = native_response_with_usage(
            "openai",
            json!({"prompt_tokens": 12, "completion_tokens": 4}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.input_tokens, 12);
        assert_eq!(ir.usage.cache_read_input_tokens, None);
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["usage"]["prompt_tokens"], json!(12));
        assert!(
            out["usage"].get("prompt_tokens_details").is_none(),
            "no spurious details: {out}"
        );
    }

    {
        let p = protocol_for("gemini").unwrap();
        let native = native_response_with_usage(
            "gemini",
            json!({"promptTokenCount": 12, "candidatesTokenCount": 4, "totalTokenCount": 16}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.cache_read_input_tokens, None);
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["usageMetadata"]["promptTokenCount"], json!(12));
        assert!(
            out["usageMetadata"]
                .get("cachedContentTokenCount")
                .is_none(),
            "no spurious cache field: {out}"
        );
    }

    {
        let p = protocol_for("responses").unwrap();
        let native = native_response_with_usage(
            "responses",
            json!({"input_tokens": 12, "output_tokens": 4}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.cache_read_input_tokens, None);
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["usage"]["input_tokens"], json!(12));

        assert_eq!(
            out["usage"]["input_tokens_details"]["cached_tokens"],
            json!(0),
            "required details present with 0 when no cache: {out}"
        );
        assert_eq!(
            out["usage"]["total_tokens"],
            json!(16),
            "total_tokens required: {out}"
        );
    }

    {
        let p = protocol_for("anthropic").unwrap();
        let native = native_response_with_usage(
            "anthropic",
            json!({"input_tokens": 12, "output_tokens": 4}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.cache_read_input_tokens, None);
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["usage"]["input_tokens"], json!(12));

        assert_eq!(
            out["usage"]["cache_read_input_tokens"],
            json!(0),
            "required cache counter present as 0 when no cache: {out}"
        );
        assert_eq!(out["usage"]["cache_creation_input_tokens"], json!(0));
    }

    {
        let p = protocol_for("bedrock").unwrap();
        let native =
            native_response_with_usage("bedrock", json!({"inputTokens": 12, "outputTokens": 4}));
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        let out = p
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        assert_eq!(out["usage"]["inputTokens"], json!(12));
        assert!(
            out["usage"].get("cacheReadInputTokens").is_none(),
            "no spurious cache field: {out}"
        );
        assert!(out["usage"].get("cacheWriteInputTokens").is_none());
    }

    {
        let p = protocol_for("cohere").unwrap();
        let native = native_response_with_usage(
            "cohere",
            json!({"tokens": {"input_tokens": 12, "output_tokens": 4}}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(ir.usage.input_tokens, 12);
        assert_eq!(ir.usage.cache_read_input_tokens, None);
        assert_eq!(ir.usage.billable_tokens(), 16);
    }
}

/// (B) BILLING PARITY + FIX. OpenAI/Gemini/Responses/Cohere: billable UNCHANGED vs before
#[test]
fn a2_billing_parity_and_fix() {
    use serde_json::json;

    {
        let p = protocol_for("openai").unwrap();
        let native = native_response_with_usage(
            "openai",
            json!({"prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 5}}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();

        assert_eq!(
            ir.usage.billable_tokens(),
            110,
            "openai cached: billable unchanged"
        );
    }
    {
        let p = protocol_for("gemini").unwrap();
        let native = native_response_with_usage(
            "gemini",
            json!({"promptTokenCount": 150, "candidatesTokenCount": 20, "cachedContentTokenCount": 30}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.billable_tokens(),
            170,
            "gemini cached: billable unchanged (150+20)"
        );
    }
    {
        let p = protocol_for("responses").unwrap();
        let native = native_response_with_usage(
            "responses",
            json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 64}}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.billable_tokens(),
            110,
            "responses cached: billable unchanged (100+10)"
        );
    }

    {
        let p = protocol_for("anthropic").unwrap();
        let native = native_response_with_usage(
            "anthropic",
            json!({"input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 5000, "cache_creation_input_tokens": 2000}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.billable_tokens(),
            7150,
            "anthropic: billable INCREASED by cache (was 150)"
        );
    }

    {
        let p = protocol_for("bedrock").unwrap();
        let native = native_response_with_usage(
            "bedrock",
            json!({"inputTokens": 100, "outputTokens": 50, "cacheReadInputTokens": 5000, "cacheWriteInputTokens": 2000}),
        );
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.billable_tokens(),
            7150,
            "bedrock: billable INCREASED by cache (was 150)"
        );
    }

    for (name, usage) in [
        (
            "anthropic",
            json!({"input_tokens": 100, "output_tokens": 50}),
        ),
        (
            "openai",
            json!({"prompt_tokens": 100, "completion_tokens": 50}),
        ),
        (
            "gemini",
            json!({"promptTokenCount": 100, "candidatesTokenCount": 50}),
        ),
        (
            "responses",
            json!({"input_tokens": 100, "output_tokens": 50}),
        ),
        ("bedrock", json!({"inputTokens": 100, "outputTokens": 50})),
        (
            "cohere",
            json!({"tokens": {"input_tokens": 100, "output_tokens": 50}}),
        ),
    ] {
        let p = protocol_for(name).unwrap();
        let native = native_response_with_usage(name, usage);
        let ir = p
            .reader()
            .read_response(crate::codec::test_host::HOST, &native)
            .unwrap();
        assert_eq!(
            ir.usage.billable_tokens(),
            150,
            "{name}: no-cache billable = input + output"
        );
    }
}

/// CROSS-PROTOCOL cache_creation reconstruction: an Anthropic/Bedrock-ingress response carries
#[test]
fn a2_cross_protocol_cache_creation_included_in_prompt_total() {
    use serde_json::json;
    let anth = protocol_for("anthropic").unwrap();
    let native = native_response_with_usage(
        "anthropic",
        json!({"input_tokens": 11, "output_tokens": 5, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 40}),
    );
    let ir = anth
        .reader()
        .read_response(crate::codec::test_host::HOST, &native)
        .unwrap();
    assert_eq!(ir.usage.input_tokens, 11);
    assert_eq!(ir.usage.cache_read_input_tokens, Some(20));
    assert_eq!(ir.usage.cache_creation_input_tokens, Some(40));

    let o = protocol_for("openai")
        .unwrap()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        o["usage"]["prompt_tokens"],
        json!(71),
        "openai prompt_total includes cache_creation"
    );
    assert_eq!(o["usage"]["total_tokens"], json!(76));

    let g = protocol_for("gemini")
        .unwrap()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        g["usageMetadata"]["promptTokenCount"],
        json!(71),
        "gemini prompt_total includes cache_creation"
    );

    let r = protocol_for("responses")
        .unwrap()
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        r["usage"]["input_tokens"],
        json!(71),
        "responses input_total includes cache_creation"
    );

    assert_eq!(ir.usage.billable_tokens(), 76);
}

/// (f) UNDERFLOW edge case: a hostile/odd upstream where cached > prompt must NOT underflow.
#[test]
fn a2_cached_exceeds_prompt_saturates_to_zero() {
    use serde_json::json;

    let p = protocol_for("openai").unwrap();
    let native = native_response_with_usage(
        "openai",
        json!({"prompt_tokens": 10, "completion_tokens": 4, "prompt_tokens_details": {"cached_tokens": 50}}),
    );
    let ir = p
        .reader()
        .read_response(crate::codec::test_host::HOST, &native)
        .unwrap();
    assert_eq!(
        ir.usage.input_tokens, 0,
        "saturating_sub clamps to 0, no underflow"
    );
    assert_eq!(ir.usage.cache_read_input_tokens, Some(50));

    let pg = protocol_for("gemini").unwrap();
    let gnative = native_response_with_usage(
        "gemini",
        json!({"promptTokenCount": 10, "candidatesTokenCount": 4, "cachedContentTokenCount": 50}),
    );
    let irg = pg
        .reader()
        .read_response(crate::codec::test_host::HOST, &gnative)
        .unwrap();
    assert_eq!(
        irg.usage.input_tokens, 0,
        "gemini saturating_sub clamps to 0"
    );

    let pr = protocol_for("responses").unwrap();
    let rnative = native_response_with_usage(
        "responses",
        json!({"input_tokens": 10, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 50}}),
    );
    let irr = pr
        .reader()
        .read_response(crate::codec::test_host::HOST, &rnative)
        .unwrap();
    assert_eq!(
        irr.usage.input_tokens, 0,
        "responses saturating_sub clamps to 0"
    );
}

/// The gemini JSON-array framer turns gemini SSE `data:` frames into one streaming JSON array
#[test]
fn test_gemini_json_array_framer_basic() {
    let mut f = GeminiJsonArrayFramer::new();
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&f.feed(b"data: {\"candidates\":[{\"index\":0}]}\n\n"));

    out.extend_from_slice(&f.feed(b"data: {\"candi"));
    out.extend_from_slice(&f.feed(b"dates\":[{\"index\":1}]}\n\n"));
    out.extend_from_slice(&f.finish());
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("framer output must be a valid JSON array");
    let arr = parsed.as_array().expect("must be an array");
    assert_eq!(arr.len(), 2, "two chunks → two array elements");
    assert_eq!(arr[0]["candidates"][0]["index"], 0);
    assert_eq!(arr[1]["candidates"][0]["index"], 1);
}

/// An empty stream (no data frame) still finishes as a valid empty JSON array `[]`, and the
#[test]
fn test_gemini_json_array_framer_empty_and_done() {
    let mut f = GeminiJsonArrayFramer::new();
    let mid = f.feed(b"data: [DONE]\n\n");
    let end = f.finish();
    let mut out = mid;
    out.extend_from_slice(&end);
    assert_eq!(out, b"[]", "empty stream → empty JSON array");
}

/// The agnostic `finish_with_server_error` seam (proxy engine:`poll_next`) reaches the framer through
#[test]
fn test_finish_with_server_error_through_trait_object_is_gemini_500_internal() {
    let mut framer: Box<dyn ArrayStreamFramer> = Box::new(gemini::GeminiJsonArrayFramer::new());
    let out = framer.finish_with_server_error("boom");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("server-error body must parse as a JSON array");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 1, "empty stream → single trailing error element");
    assert_eq!(arr[0]["error"]["code"], 500);
    assert_eq!(arr[0]["error"]["status"], "INTERNAL");
    assert_eq!(arr[0]["error"]["message"], "boom");
}

/// `finish_with_error` after real chunks appends a gemini-shaped error element + `]`, so
#[test]
fn test_gemini_json_array_framer_finish_with_error_closes_array() {
    let mut f = GeminiJsonArrayFramer::new();
    let mut out = f.feed(b"data: {\"candidates\":[{\"index\":0}]}\n\n");
    out.extend_from_slice(&f.finish_with_error(500, "INTERNAL", "boom"));
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("error-terminated body must parse as JSON array");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 2, "one chunk + one trailing error element");
    assert_eq!(arr[1]["error"]["code"], 500);
    assert_eq!(arr[1]["error"]["status"], "INTERNAL");

    let mut g = GeminiJsonArrayFramer::new();
    let only = g.finish_with_error(503, "UNAVAILABLE", "x");
    let pv: serde_json::Value = serde_json::from_slice(&only).expect("parses");
    assert_eq!(pv.as_array().expect("array").len(), 1);
}

/// When the framer ABORTS (reassembly buffer overran `MAX_BUF` without a terminator),
#[test]
fn test_gemini_json_array_framer_finish_signals_abort() {
    let mut f = GeminiJsonArrayFramer::new();

    let huge = vec![b'x'; GeminiJsonArrayFramer::MAX_BUF + 16];
    let mut pre = Vec::from(&b"data: {\"k\":\""[..]);
    pre.extend_from_slice(&huge);
    let _ = f.feed(&pre);
    let out = f.finish();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("aborted finish must still parse as JSON array");
    let arr = parsed.as_array().expect("array");
    assert!(
        arr.iter().any(|el| el.get("error").is_some()),
        "aborted stream must surface an error element, not a silent bare close; got {parsed}"
    );
}

/// Regression: on the GEMINI JSON-ARRAY ingress path the buffer-overflow abort of the
#[test]
fn test_gemini_json_array_surfaces_upstream_translate_abort() {
    let mut st = StreamTranslate::new("gemini", "openai").expect("translator");

    let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}]}\n\n";
    let mut framer = GeminiJsonArrayFramer::new();
    let translated = st.feed(crate::codec::test_host::HOST, chunk);
    let mut out = framer.feed(&translated);

    let huge = vec![b'x'; StreamTranslate::MAX_BUF + 16];
    let mut never_terminated = Vec::from(&b"data: {\"choices\":[{\"delta\":{\"content\":\""[..]);
    never_terminated.extend_from_slice(&huge);
    let _ = st.feed(crate::codec::test_host::HOST, &never_terminated);
    assert!(
        st.aborted(),
        "the translate must have aborted on the MAX_BUF overflow"
    );

    out.extend_from_slice(&framer.finish_for_translate(st.aborted()));
    let parsed: serde_json::Value = serde_json::from_slice(&out)
        .expect("aborted-translate finish must still parse as a JSON array");
    let arr = parsed.as_array().expect("array");
    assert!(
            arr.iter().any(|el| el.get("error").is_some()),
            "an aborted upstream translate must surface an error element, not a silent bare close; got {parsed}"
        );

    let mut clean_st = StreamTranslate::new("gemini", "openai").expect("translator");
    let mut clean_framer = GeminiJsonArrayFramer::new();
    let mut clean_out = clean_framer.feed(&clean_st.feed(crate::codec::test_host::HOST, chunk));
    let _ = clean_st.finish(crate::codec::test_host::HOST);
    clean_out.extend_from_slice(&clean_framer.finish_for_translate(clean_st.aborted()));
    let clean: serde_json::Value = serde_json::from_slice(&clean_out).expect("clean finish parses");
    assert!(
        clean
            .as_array()
            .expect("array")
            .iter()
            .all(|el| el.get("error").is_none()),
        "a non-aborted stream must close cleanly with no error element; got {clean}"
    );
}

/// A MALFORMED Bedrock EGRESS
#[test]
fn test_egress_eventstream_malformed_prelude_aborts_and_surfaces_error() {
    let mut st = StreamTranslate::new("anthropic", "bedrock").expect("translator");

    let mut malformed = Vec::new();
    malformed.extend_from_slice(&u32::MAX.to_be_bytes());
    malformed.extend_from_slice(&0u32.to_be_bytes());
    malformed.extend_from_slice(&[0, 0, 0, 0]);
    let out = st.feed(crate::codec::test_host::HOST, &malformed);
    assert!(
        out.is_empty(),
        "a malformed prelude yields no translated frames; got {} bytes",
        out.len()
    );
    assert!(
        st.aborted(),
        "a malformed egress prelude must abort the stream, not silently continue"
    );

    assert!(
        st.feed(crate::codec::test_host::HOST, b"anything")
            .is_empty(),
        "an aborted translator must ignore all further input"
    );

    let term = st.finish(crate::codec::test_host::HOST);
    let text = String::from_utf8_lossy(&term);
    assert!(
        text.contains("error"),
        "an aborted stream's finish must emit a native error event, not a bare close; got: {text}"
    );
}

/// On a SAME-PROTOCOL
#[test]
fn same_proto_bedrock_malformed_prelude_emits_only_valid_frames_not_garbage() {
    let mut st = StreamTranslate::new_same_proto("bedrock").expect("same-proto translator");

    let valid =
        crate::codec::eventstream::encode_frame("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
    let mut bytes = valid.clone();
    bytes.extend_from_slice(&u32::MAX.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(b"GARBAGE-MUST-NOT-REACH-CLIENT");
    let out = st.feed(crate::codec::test_host::HOST, &bytes);
    assert_eq!(
        out, valid,
        "same-proto verbatim emit must forward ONLY the valid frame bytes, never the cleared \
             malformed remainder"
    );
    assert!(
        !out.windows(7).any(|w| w == b"GARBAGE"),
        "the malformed garbage tail must NOT appear in the client stream"
    );
    assert!(st.aborted(), "a malformed prelude aborts the stream");

    assert!(
        !st.finish(crate::codec::test_host::HOST).is_empty(),
        "an aborted same-proto bedrock stream must emit a terminal exception frame"
    );
}

/// Gemini URL sources have no Anthropic encrypted search index. A multi-citation batch must
#[test]
fn stream_gemini_multi_citation_without_anthropic_state_is_audited() {
    let mut st = StreamTranslate::new("anthropic", "gemini").expect("translator");

    let chunk = br#"data: {"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"answer text"}]},"finishReason":"STOP","citationMetadata":{"citationSources":[{"startIndex":0,"endIndex":3,"uri":"https://example.com/a","title":"A"},{"startIndex":3,"endIndex":6,"uri":"https://example.com/b","title":"B"},{"startIndex":6,"endIndex":9,"uri":"https://example.com/c","title":"C"}]}}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":9,"totalTokenCount":16}}

"#;
    let mut out = st.feed(crate::codec::test_host::HOST, chunk);
    out.extend_from_slice(&st.finish(crate::codec::test_host::HOST));
    let text = String::from_utf8(out).expect("anthropic SSE is utf-8");

    let mut answer = String::new();
    for line in text.lines() {
        let Some(json) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<serde_json::Value>(json.trim()) else {
            continue;
        };
        assert!(
            !body.is_array(),
            "no SSE event body may be a JSON array (a native Anthropic SDK crashes on it): {body}"
        );
        assert_ne!(
            body.pointer("/delta/type").and_then(|t| t.as_str()),
            Some("citations_delta"),
            "unrepresentable source cannot become a native citation: {body}"
        );
        if let Some(delta) = body.pointer("/delta/text").and_then(|text| text.as_str()) {
            answer.push_str(delta);
        }
    }

    assert_eq!(answer, "answer text", "{text}");
    assert!(!text.contains("encrypted_index"), "{text}");
    assert!(!st.dropped().is_empty(), "citation loss must be audited");
    let usage = st.usage().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 9));
}

/// CF6 (wire-conformance): the same Gemini multi-citation fan-out, but egressing into the COHERE v2
#[test]
fn stream_gemini_multi_citation_yields_separate_single_object_cohere_events() {
    let mut st = StreamTranslate::new("cohere", "gemini").expect("translator");
    let chunk = br#"data: {"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"answer text"}]},"citationMetadata":{"citationSources":[{"startIndex":0,"endIndex":3,"uri":"https://example.com/a","title":"A"},{"startIndex":3,"endIndex":6,"uri":"https://example.com/b","title":"B"},{"startIndex":6,"endIndex":9,"uri":"https://example.com/c","title":"C"}]}}]}

"#;
    let mut out = st.feed(crate::codec::test_host::HOST, chunk);
    out.extend_from_slice(&st.finish(crate::codec::test_host::HOST));
    let text = String::from_utf8(out).expect("cohere SSE is utf-8");

    let mut citation_bodies: Vec<serde_json::Value> = Vec::new();
    for line in text.lines() {
        let Some(json) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<serde_json::Value>(json.trim()) else {
            continue;
        };

        assert!(
            !body.is_array(),
            "no SSE event body may be a JSON array: {body}"
        );
        if body.get("type").and_then(|t| t.as_str()) == Some("citation-start") {
            let citations = body
                .pointer("/delta/message/citations")
                .expect("a citation-start carries delta.message.citations");

            assert!(
                citations.is_object() && !citations.is_array(),
                "citation-start `delta.message.citations` must be a single Citation object, not an \
                 array: {citations}"
            );
            citation_bodies.push(body);
        }
    }

    assert_eq!(
        citation_bodies.len(),
        3,
        "3 Gemini citationSources → 3 separate single-object Cohere citation-start events; got {text}"
    );

    let urls: Vec<String> = citation_bodies
        .iter()
        .filter_map(|b| {
            b.pointer("/delta/message/citations/sources/0/document/url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(
        urls,
        vec![
            "https://example.com/a".to_string(),
            "https://example.com/b".to_string(),
            "https://example.com/c".to_string()
        ],
        "each fanned-out citation-start carries its own source url; got {text}"
    );
}

/// HIGH-1 reverse direction: 3 Anthropic single-citation `citations_delta` events (as a native
#[test]
fn stream_anthropic_single_citations_project_to_valid_gemini_citation_metadata() {
    let writer = GeminiWriter;
    let mk = |url: &str| crate::codec::ir::IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::CitationsDelta(vec![crate::codec::ir::IrCitation {
            domain: None,
            kind: Some("web_search_result_location".to_string()),
            cited_text: None,
            title: Some("T".to_string()),
            url: Some(url.to_string()),
            document_index: None,
            start_index: Some(0),
            end_index: Some(3),
            encrypted_index: None,
            raw: None,
            ..Default::default()
        }]),
    };
    for url in ["https://x/1", "https://x/2", "https://x/3"] {
        let (_, body) = writer
            .write_response_event(crate::codec::test_host::HOST, &mk(url))
            .expect("each single-citation delta emits a Gemini citationMetadata chunk");
        let sources = body
            .pointer("/candidates/0/citationMetadata/citationSources")
            .and_then(|s| s.as_array())
            .expect("a valid Gemini chunk carries candidate-level citationSources[]");
        assert_eq!(sources.len(), 1, "one citation per delta → one source");
        assert_eq!(
            sources[0].get("uri").and_then(|v| v.as_str()),
            Some(url),
            "the Gemini source carries the citation uri"
        );
    }
}

/// A REDACTED reasoning block (`Thinking { redacted: true }`) holds opaque encrypted bytes. On a
#[test]
fn redacted_reasoning_drops_on_writers_without_a_native_form() {
    let ir = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::Thinking {
            text: "OPAQUEENCRYPTEDBYTES".to_string(),
            signature: None,
            redacted: true,
            cache_control: None,
            kind: None,
            signature_origin: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
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

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let (gw, rw, cw) = (GeminiWriter, ResponsesWriter, CohereWriter);
    let gemini = gw
        .write_response(crate::codec::test_host::HOST, &ir)
        .to_string();
    let responses = rw
        .write_response(crate::codec::test_host::HOST, &ir)
        .to_string();
    let openai = openai_writer()
        .write_response(crate::codec::test_host::HOST, &ir)
        .to_string();
    let cohere = cw
        .write_response(crate::codec::test_host::HOST, &ir)
        .to_string();
    for (name, wire) in [
        ("gemini", &gemini),
        ("responses", &responses),
        ("openai", &openai),
        ("cohere", &cohere),
    ] {
        assert!(
                !wire.contains("OPAQUEENCRYPTEDBYTES"),
                "{name} must DROP a redacted block — the opaque bytes must never reach the wire: {wire}"
            );
    }

    let redacted_delta = crate::codec::ir::IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::RedactedReasoningDelta(
            "OPAQUEENCRYPTEDBYTES".to_string(),
        ),
    };
    assert!(
        gw.write_response_event(crate::codec::test_host::HOST, &redacted_delta)
            .is_none(),
        "gemini stream must drop a redacted-reasoning delta"
    );
}

/// Encode one AWS event-stream frame (`:event-type` string header + JSON payload) for tests.
fn es_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
    let name = b":event-type";
    let mut headers = vec![name.len() as u8];
    headers.extend_from_slice(name);
    headers.push(7);
    headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
    headers.extend_from_slice(event_type.as_bytes());
    let total = 12 + headers.len() + payload.len() + 4;
    let mut f = Vec::new();
    f.extend_from_slice(&(total as u32).to_be_bytes());
    f.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.extend_from_slice(&headers);
    f.extend_from_slice(payload);
    f.extend_from_slice(&[0, 0, 0, 0]);
    f
}

/// HIGH/conformance regression (eventstream.rs): a Bedrock EGRESS that sends a mid-stream
#[test]
fn test_translate_bedrock_egress_exception_frame_surfaces_error_to_ingress() {
    let mut st = StreamTranslate::new("anthropic", "bedrock").expect("bedrock egress translator");
    let mut bytes = es_frame("messageStart", br#"{"role":"assistant"}"#);

    bytes.extend(crate::codec::eventstream::encode_exception_frame(
        "ThrottlingException",
        "rate exceeded mid-stream",
    ));

    let out = String::from_utf8(st.feed(crate::codec::test_host::HOST, &bytes)).unwrap();

    assert!(
        out.contains("event: error") || out.contains("\"type\":\"error\""),
        "bedrock-egress mid-stream exception must translate to an ingress error event; got:\n{out}"
    );

    assert!(
        out.contains("rate exceeded mid-stream"),
        "the exception message must reach the ingress error body; got:\n{out}"
    );
}

/// a Bedrock ConverseStream (binary event-stream egress) translates to Anthropic SSE for
#[test]
fn test_translate_bedrock_eventstream_egress_to_anthropic_ingress() {
    let mut st = StreamTranslate::new("anthropic", "bedrock").expect("bedrock egress translator");
    let mut bytes = es_frame("messageStart", br#"{"role":"assistant"}"#);
    bytes.extend(es_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#,
    ));
    bytes.extend(es_frame("contentBlockStop", br#"{"contentBlockIndex":0}"#));
    bytes.extend(es_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
    bytes.extend(es_frame(
        "metadata",
        br#"{"usage":{"inputTokens":5,"outputTokens":2}}"#,
    ));

    let mut out = String::from_utf8(st.feed(crate::codec::test_host::HOST, &bytes)).unwrap();
    out.push_str(&String::from_utf8(st.finish(crate::codec::test_host::HOST)).unwrap());

    assert!(out.contains("event: message_start"), "got:\n{out}");

    assert!(
        out.contains("\"text\":\"Hi\""),
        "text delta must ride the anthropic `text` key; got:\n{out}"
    );
    assert!(out.contains("message_stop"), "terminator; got:\n{out}");

    let delta_pos = out.find("event: message_delta");
    let stop_pos = out.find("event: message_stop");
    assert!(
        delta_pos.is_some() && stop_pos.is_some() && delta_pos < stop_pos,
        "message_delta must precede message_stop (native order); got:\n{out}"
    );

    assert!(
        out.contains("\"type\":\"message_delta\""),
        "message_delta data body must carry top-level type; got:\n{out}"
    );
    assert!(
        out.contains("\"type\":\"message_stop\""),
        "message_stop data body must carry top-level type; got:\n{out}"
    );

    assert!(
        out.contains("\"input_tokens\":5") && out.contains("\"output_tokens\":2"),
        "combined message_delta must carry the Bedrock metadata usage; got:\n{out}"
    );
}

/// Chat#5 (end-to-end): a STREAMING Bedrock-backend → Anthropic-client turn carrying REDACTED
#[test]
fn test_bedrock_egress_redacted_reasoning_preserved_to_anthropic_ingress() {
    let mut st = StreamTranslate::new("anthropic", "bedrock").expect("bedrock egress translator");
    let mut bytes = es_frame("messageStart", br#"{"role":"assistant"}"#);

    bytes.extend(es_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"OPAQUE_CIPHERTEXT_BYTES"}}}"#,
    ));
    bytes.extend(es_frame("contentBlockStop", br#"{"contentBlockIndex":0}"#));

    bytes.extend(es_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":1,"delta":{"text":"Done."}}"#,
    ));
    bytes.extend(es_frame("contentBlockStop", br#"{"contentBlockIndex":1}"#));
    bytes.extend(es_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
    bytes.extend(es_frame(
        "metadata",
        br#"{"usage":{"inputTokens":5,"outputTokens":2}}"#,
    ));

    let mut out = String::from_utf8(st.feed(crate::codec::test_host::HOST, &bytes)).unwrap();
    out.push_str(&String::from_utf8(st.finish(crate::codec::test_host::HOST)).unwrap());

    assert!(
        out.contains("\"type\":\"redacted_thinking\""),
        "the client must receive a native redacted_thinking block; got:\n{out}"
    );
    assert!(
        out.contains("OPAQUE_CIPHERTEXT_BYTES"),
        "the opaque encrypted reasoning bytes must be PRESERVED end-to-end (the reasoning-reuse blob); \
         got:\n{out}"
    );

    assert!(
        !out.contains("\"type\":\"thinking\""),
        "a redacted block must never be emitted as a plaintext thinking block; got:\n{out}"
    );

    assert!(
        out.contains("Done."),
        "the visible answer must survive; got:\n{out}"
    );
}

/// Delta-before-stop regression at the reader→writer level (independent of eventstream framing): the
#[test]
fn test_bedrock_reader_emits_delta_before_stop_for_anthropic_ingress() {
    use crate::codec::ir::IrStreamEvent;
    let reader = BedrockReader;
    let writer = AnthropicWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let mut events: Vec<IrStreamEvent> = Vec::new();
    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        &mut state,
    ));
    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "metadata", "usage": {"inputTokens": 5, "outputTokens": 2}}),
        &mut state,
    ));

    assert!(
        matches!(events.first(), Some(IrStreamEvent::MessageDelta { .. })),
        "combined MessageDelta must come first; got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(IrStreamEvent::MessageStop)),
        "terminal MessageStop must come last; got {events:?}"
    );

    let wire: Vec<String> = events
        .iter()
        .filter_map(|e| {
            writer
                .write_response_event(crate::codec::test_host::HOST, e)
                .map(|(et, _)| et)
        })
        .collect();
    let delta_pos = wire.iter().position(|t| t == "message_delta");
    let stop_pos = wire.iter().position(|t| t == "message_stop");
    assert!(
        delta_pos.is_some() && stop_pos.is_some() && delta_pos < stop_pos,
        "Anthropic writer must emit message_delta before message_stop; got {wire:?}"
    );
}

/// Regression: the post-`MessageStop` ordering guard must drop a DUPLICATE terminal
#[test]
fn test_duplicate_terminal_message_delta_after_stop_is_dropped() {
    let mut st = StreamTranslate::new("openai", "anthropic").expect("translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",

            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
        ] {
            raw.extend(st.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }

    let out = String::from_utf8(raw).expect("utf8 SSE");

    let finishes = out.matches("\"finish_reason\":\"stop\"").count();
    assert_eq!(
            finishes, 1,
            "exactly one terminal finish_reason chunk; the duplicate terminal delta must be dropped; got:\n{out}"
        );
}

/// BYTE-IDENTITY GUARD: locks the wire shape of the OpenAI chunk-identity replay +
#[test]
fn openai_egress_chunk_identity_and_trailing_usage_byte_shape() {
    let mut st = StreamTranslate::new("openai", "anthropic").expect("translator");

    st.set_client_include_usage(true);
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(st.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    let ids: std::collections::BTreeSet<String> = out
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| d.contains("chat.completion.chunk"))
        .filter_map(|d| {
            let start = d.find("\"id\":\"")? + 6;
            let rest = &d[start..];
            Some(rest[..rest.find('"')?].to_string())
        })
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "all openai chunks must share ONE stream id; got {ids:?} in\n{out}"
    );
    assert!(
        !out.contains("msg_x"),
        "the anthropic backend id must not leak; got\n{out}"
    );

    assert!(
        out.contains("\"finish_reason\":\"stop\""),
        "a terminal finish chunk; got\n{out}"
    );
    assert!(
        out.contains("\"usage\""),
        "a trailing usage chunk; got\n{out}"
    );
    for line in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let has_finish = line.contains("\"finish_reason\":\"stop\"");
        let has_usage = line.contains("\"usage\":{");
        assert!(
            !(has_finish && has_usage),
            "no single chunk may carry BOTH finish_reason=stop AND usage (un-fold); got:\n{line}"
        );
    }
}

/// Unsolicited trailing usage chunk: a client that did NOT send
#[test]
fn openai_egress_without_include_usage_emits_no_usage_chunk() {
    let mut st = StreamTranslate::new("openai", "anthropic").expect("translator");

    st.set_client_include_usage(false);
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(st.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    assert!(
        out.contains("\"finish_reason\":\"end_turn\"")
            || out.contains("\"finish_reason\":\"stop\""),
        "a terminal finish chunk must still be emitted; got\n{out}"
    );

    for data in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if data.trim() == "[DONE]" {
            continue;
        }
        let v: serde_json::Value =
            crate::codec::json::parse(data.as_bytes()).expect("each SSE chunk is valid JSON");
        assert!(
            v.get("usage").is_none(),
            "an opted-out client must receive NO usage object; got:\n{data}"
        );
        assert!(
            v.pointer("/choices")
                .and_then(|c| c.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(true),
            "an opted-out client must receive NO empty-choices trailing usage chunk; got:\n{data}"
        );
    }

    let billed = st
        .usage()
        .expect("the A-tap must capture usage for billing");
    assert_eq!(
        billed.input_tokens, 5,
        "billing keeps the real input tokens"
    );
    assert_eq!(
        billed.output_tokens, 2,
        "billing keeps the real output tokens"
    );
}

/// 0-based streaming tool_calls index: an Anthropic backend stream with a leading text
#[test]
fn openai_egress_multi_tool_stream_uses_0_based_tool_call_indices() {
    let mut st = StreamTranslate::new("openai", "anthropic").expect("translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",

            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me look those up.\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",

            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"NYC\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",

            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"get_time\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"tz\\\":\\\"ET\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(st.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    let mut opens: Vec<(String, i64)> = Vec::new();
    let mut all_indices: Vec<i64> = Vec::new();
    for data in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let Ok(v) = crate::codec::json::parse::<serde_json::Value>(data.as_bytes()) else {
            continue;
        };
        let Some(calls) = v
            .pointer("/choices/0/delta/tool_calls")
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in calls {
            let idx = c
                .get("index")
                .and_then(|i| i.as_i64())
                .expect("tool_call index");
            all_indices.push(idx);
            if let Some(name) = c.pointer("/function/name").and_then(|n| n.as_str()) {
                opens.push((name.to_string(), idx));
            }
        }
    }

    let first = opens
        .iter()
        .find(|(name, _)| name == "get_weather")
        .expect("first tool call open chunk");
    assert_eq!(
        first.1, 0,
        "the FIRST tool call must be tool_calls index 0 (raw IR block index 1); got\n{out}"
    );

    let second = opens
        .iter()
        .find(|(name, _)| name == "get_time")
        .expect("second tool call open chunk");
    assert_eq!(
        second.1, 1,
        "the SECOND tool call must be tool_calls index 1 (raw IR block index 2); got\n{out}"
    );

    assert!(
        all_indices.iter().all(|idx| *idx == 0 || *idx == 1),
        "tool_calls indices must be the 0-based ordinals {{0,1}}, never the raw block indices; got {all_indices:?}\n{out}"
    );
}

/// BYTE-IDENTITY GUARD: locks the wire shape of the Bedrock messageStop/metadata two-frame
#[test]
fn bedrock_egress_emits_exactly_one_metadata_frame() {
    let mut a = StreamTranslate::new("bedrock", "anthropic").expect("translator");
    let mut raw_a: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw_a.extend(a.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw_a.extend(a.finish(crate::codec::test_host::HOST));
    let mut buf_a = raw_a;
    let frames_a = crate::codec::eventstream::drain_frames(&mut buf_a);
    assert!(
        buf_a.is_empty(),
        "case A frames must decode cleanly; {} left",
        buf_a.len()
    );
    let meta_a = frames_a.iter().filter(|(et, _)| et == "metadata").count();
    assert_eq!(
        meta_a, 1,
        "case A must emit exactly ONE metadata frame; got {meta_a}"
    );

    let mut b = StreamTranslate::new("bedrock", "openai").expect("translator");
    let mut raw_b: Vec<u8> = Vec::new();
    for frame in [
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ] {
            raw_b.extend(b.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    raw_b.extend(b.finish(crate::codec::test_host::HOST));
    let mut buf_b = raw_b;
    let frames_b = crate::codec::eventstream::drain_frames(&mut buf_b);
    assert!(
        buf_b.is_empty(),
        "case B frames must decode cleanly; {} left",
        buf_b.len()
    );
    let meta_b = frames_b.iter().filter(|(et, _)| et == "metadata").count();
    assert_eq!(
        meta_b, 1,
        "case B (deferred) must flush exactly ONE metadata frame via finish(); got {meta_b}"
    );
}

/// Bedrock *ingress* streaming: an Anthropic SSE backend stream → a native AWS SDK Bedrock
#[test]
fn test_translate_anthropic_egress_to_bedrock_ingress_binary_frames() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_backend\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }

    assert!(
        t.finish(crate::codec::test_host::HOST).is_empty(),
        "bedrock ingress emits no terminator frame in finish()"
    );

    assert!(
        !raw.starts_with(b"event:") && !raw.starts_with(b"data:"),
        "bedrock ingress output must be binary frames, not SSE"
    );

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all emitted frames must decode cleanly (valid CRC + lengths); {} bytes left",
        buf.len()
    );
    let types: Vec<&str> = frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        types.first().copied(),
        Some("messageStart"),
        "stream opens with messageStart; got {types:?}"
    );
    assert!(
        types.contains(&"contentBlockDelta"),
        "must carry a contentBlockDelta; got {types:?}"
    );
    assert!(
        types.contains(&"messageStop"),
        "must carry messageStop terminator; got {types:?}"
    );

    assert!(
        types.contains(&"metadata"),
        "combined delta must fan out a `metadata` usage frame; got {types:?}"
    );

    let stop_pos = types.iter().position(|t| *t == "messageStop");
    let meta_pos = types.iter().position(|t| *t == "metadata");
    assert!(
        stop_pos < meta_pos,
        "messageStop must precede metadata (native order); got {types:?}"
    );

    let meta = frames
        .iter()
        .find(|(et, _)| et == "metadata")
        .expect("a metadata frame");
    let mv: serde_json::Value =
        serde_json::from_slice(&meta.1).expect("valid metadata JSON payload");
    assert_eq!(
        mv.pointer("/usage/inputTokens").and_then(|x| x.as_u64()),
        Some(5),
        "metadata usage inputTokens round-trips; got {mv}"
    );
    assert_eq!(
        mv.pointer("/usage/outputTokens").and_then(|x| x.as_u64()),
        Some(2),
        "metadata usage outputTokens round-trips; got {mv}"
    );

    assert!(
        mv.pointer("/metrics/latencyMs")
            .and_then(|x| x.as_u64())
            .is_some(),
        "metadata must carry a real metrics.latencyMs; got {mv}"
    );

    let delta = frames
        .iter()
        .find(|(et, _)| et == "contentBlockDelta")
        .expect("a contentBlockDelta frame");
    let v: serde_json::Value = serde_json::from_slice(&delta.1).expect("valid JSON payload");
    assert_eq!(
        v.pointer("/delta/text").and_then(|x| x.as_str()),
        Some("Hi"),
        "delta text round-trips; got {v}"
    );

    assert!(
        !raw.windows(b"msg_backend".len())
            .any(|w| w == b"msg_backend"),
        "foreign backend stream id must be stripped on cross-protocol ingress"
    );
}

/// CF7 (wire-conformance): a cross-protocol TEXT stream egressing into Bedrock ConverseStream must
#[test]
fn test_translate_anthropic_text_to_bedrock_ingress_has_no_content_block_start() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all frames decode cleanly; {} bytes left",
        buf.len()
    );
    let block_frames: Vec<&str> = frames
        .iter()
        .map(|(et, _)| et.as_str())
        .filter(|et| et.starts_with("contentBlock"))
        .collect();
    assert!(
        !block_frames.contains(&"contentBlockStart"),
        "a cross-protocol text egress into Bedrock must emit NO contentBlockStart; got {block_frames:?}"
    );

    assert_eq!(
        block_frames,
        vec!["contentBlockDelta", "contentBlockStop"],
        "text block frames must be exactly delta then stop; got {block_frames:?}"
    );

    let delta = frames
        .iter()
        .find(|(et, _)| et == "contentBlockDelta")
        .expect("a contentBlockDelta frame");
    let v: serde_json::Value = serde_json::from_slice(&delta.1).expect("valid JSON payload");
    assert_eq!(
        v.pointer("/delta/text").and_then(|x| x.as_str()),
        Some("Hello")
    );
}

/// CF8 (wire-conformance): a cross-protocol REASONING stream egressing into Bedrock ConverseStream
#[test]
fn test_translate_anthropic_reasoning_to_bedrock_ingress_uses_delta_not_start() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me think\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all frames decode cleanly; {} bytes left",
        buf.len()
    );
    let block_frames: Vec<&str> = frames
        .iter()
        .map(|(et, _)| et.as_str())
        .filter(|et| et.starts_with("contentBlock"))
        .collect();
    assert!(
        !block_frames.contains(&"contentBlockStart"),
        "a cross-protocol reasoning egress into Bedrock must emit NO contentBlockStart; got {block_frames:?}"
    );

    let reasoning_text: String = frames
        .iter()
        .filter(|(et, _)| et == "contentBlockDelta")
        .filter_map(|(_, body)| serde_json::from_slice::<serde_json::Value>(body).ok())
        .filter_map(|v| {
            v.pointer("/delta/reasoningContent/text")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(
        reasoning_text, "let me think",
        "the reasoning text must ride contentBlockDelta.reasoningContent.text; got {block_frames:?}"
    );
    let has_signature = frames
        .iter()
        .filter(|(et, _)| et == "contentBlockDelta")
        .filter_map(|(_, body)| serde_json::from_slice::<serde_json::Value>(body).ok())
        .any(|v| {
            v.pointer("/delta/reasoningContent/signature")
                .and_then(|x| x.as_str())
                == Some("sig-xyz")
        });
    assert!(
        has_signature,
        "the reasoning signature must ride contentBlockDelta.reasoningContent.signature"
    );

    assert!(
        block_frames.contains(&"contentBlockStop"),
        "the reasoning block must close with a contentBlockStop; got {block_frames:?}"
    );
}

/// Bedrock *ingress* streaming, TOOL-CALL path: an Anthropic SSE `content_block_start` with a
#[test]
fn test_translate_anthropic_egress_to_bedrock_ingress_tool_call() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"SF\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all emitted frames decode cleanly; {} bytes left",
        buf.len()
    );
    let types: Vec<&str> = frames.iter().map(|(et, _)| et.as_str()).collect();
    assert!(
        types.contains(&"contentBlockStart"),
        "tool_use must emit a contentBlockStart frame; got {types:?}"
    );
    assert!(
        types.contains(&"contentBlockStop"),
        "must emit a contentBlockStop frame; got {types:?}"
    );

    let start = frames
        .iter()
        .find(|(et, _)| et == "contentBlockStart")
        .expect("a contentBlockStart frame");
    let v: serde_json::Value = serde_json::from_slice(&start.1).expect("valid JSON payload");
    assert_eq!(
        v.pointer("/start/toolUse/name").and_then(|x| x.as_str()),
        Some("get_weather"),
        "toolUse name round-trips; got {v}"
    );

    let emitted_tool_id = v
        .pointer("/start/toolUse/toolUseId")
        .and_then(|x| x.as_str())
        .expect("toolUseId present");
    assert!(
            emitted_tool_id.starts_with("tooluse_") && emitted_tool_id != "toolu_abc",
            "Bedrock client must see a native `tooluse_` id, not the foreign `toolu_abc`; got {emitted_tool_id}"
        );
    assert_eq!(
        decode_native_tool_id("bedrock", emitted_tool_id).as_deref(),
        Some("toolu_abc"),
        "the reshaped id must decode back to the original egress tool id; got {emitted_tool_id}"
    );

    let delta = frames
        .iter()
        .find(|(et, _)| et == "contentBlockDelta")
        .expect("a contentBlockDelta frame");
    let dv: serde_json::Value = serde_json::from_slice(&delta.1).expect("valid JSON payload");
    assert!(
        dv.pointer("/delta/toolUse/input").is_some(),
        "tool input delta round-trips through the binary encoder; got {dv}"
    );
}

/// INV-A: a Cohere stream truncated after `tool-call-start`/`tool-call-delta` (no `tool-call-end`),
#[test]
fn cohere_stream_truncated_tool_call_is_closed_before_message_stop() {
    let mut t = StreamTranslate::new("anthropic", "cohere").expect("anthropic ingress translator");
    let mut out = String::new();
    for frame in [
        serde_json::json!({"type": "message-start", "id": "run-1"}),
        serde_json::json!({
            "type": "tool-call-start",
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }}}
        }),
        serde_json::json!({
            "type": "tool-call-delta",
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "function": {"arguments": "{\"city\":\"SF\"}"}
            }}}
        }),
        serde_json::json!({
            "type": "message-end",
            "delta": {"finish_reason": "COMPLETE", "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}}
        }),
    ] {
        out.push_str(&String::from_utf8_lossy(
            &t.feed(
                crate::codec::test_host::HOST,
                format!(
                    "event: {}\ndata: {}\n\n",
                    frame["type"].as_str().unwrap(),
                    frame
                )
                .as_bytes(),
            ),
        ));
    }
    out.push_str(&String::from_utf8_lossy(
        &t.finish(crate::codec::test_host::HOST),
    ));

    let payloads = data_payloads(&out);
    let types: Vec<&str> = payloads
        .iter()
        .filter_map(|p| p.get("type").and_then(|t| t.as_str()))
        .collect();

    let stop_pos = types.iter().position(|t| *t == "content_block_stop");
    let message_delta_pos = types.iter().position(|t| *t == "message_delta");
    let message_stop_pos = types.iter().position(|t| *t == "message_stop");

    assert!(
        stop_pos.is_some(),
        "the truncated tool block must be closed with a content_block_stop; got types {types:?}\n{out}"
    );
    assert!(
        stop_pos < message_delta_pos,
        "content_block_stop must precede message_delta; got types {types:?}\n{out}"
    );
    assert!(
        stop_pos < message_stop_pos,
        "content_block_stop must precede message_stop; got types {types:?}\n{out}"
    );
}

/// INV-A: the same truncation hole exists in every reader's terminal branch when the terminal frame
#[test]
fn gemini_stream_truncated_before_finish_reason_closes_blocks() {
    let mut t = StreamTranslate::new("anthropic", "gemini").expect("anthropic ingress translator");
    let mut out = String::new();
    out.push_str(&String::from_utf8_lossy(&t.feed(crate::codec::test_host::HOST,
        b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n",
    )));

    out.push_str(&String::from_utf8_lossy(
        &t.finish(crate::codec::test_host::HOST),
    ));

    let payloads = data_payloads(&out);
    let starts: Vec<u64> = payloads
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("content_block_start"))
        .filter_map(|p| p.get("index").and_then(|i| i.as_u64()))
        .collect();
    let stops: Vec<u64> = payloads
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("content_block_stop"))
        .filter_map(|p| p.get("index").and_then(|i| i.as_u64()))
        .collect();

    assert!(
        !starts.is_empty(),
        "expected at least one content_block_start; got\n{out}"
    );
    assert_eq!(
        starts, stops,
        "every content_block_start must have a matching content_block_stop even with no finishReason; got\n{out}"
    );
}

/// INV-A: same truncation hole in the OpenAI reader — a tool_calls stream that never reaches its
#[test]
fn openai_stream_truncated_tool_call_is_closed() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("anthropic ingress translator");
    let mut out = String::new();
    out.push_str(&String::from_utf8_lossy(&t.feed(crate::codec::test_host::HOST,
        b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n",
    )));
    out.push_str(&String::from_utf8_lossy(&t.feed(crate::codec::test_host::HOST,
        b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}}]}\n\n",
    )));

    out.push_str(&String::from_utf8_lossy(
        &t.finish(crate::codec::test_host::HOST),
    ));

    let payloads = data_payloads(&out);
    let starts: Vec<u64> = payloads
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("content_block_start"))
        .filter_map(|p| p.get("index").and_then(|i| i.as_u64()))
        .collect();
    let stops: Vec<u64> = payloads
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("content_block_stop"))
        .filter_map(|p| p.get("index").and_then(|i| i.as_u64()))
        .collect();

    assert!(
        !starts.is_empty(),
        "expected at least one content_block_start; got\n{out}"
    );
    assert_eq!(
        starts, stops,
        "every content_block_start must have a matching content_block_stop even with no finish_reason; got\n{out}"
    );
}

/// INV-A, and the specific guard against the fatal v1 bug: tracking must happen on the IR event
#[test]
fn cohere_truncated_tool_call_survives_gemini_ingress() {
    let mut t = StreamTranslate::new("gemini", "cohere").expect("gemini ingress translator");
    let mut out = String::new();
    for frame in [
        serde_json::json!({"type": "message-start", "id": "run-1"}),
        serde_json::json!({
            "type": "tool-call-start",
            "index": 0,
            "delta": {"message": {"tool_calls": {
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}
            }}}
        }),
        serde_json::json!({
            "type": "message-end",
            "delta": {"finish_reason": "COMPLETE", "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}}
        }),
    ] {
        out.push_str(&String::from_utf8_lossy(
            &t.feed(
                crate::codec::test_host::HOST,
                format!(
                    "event: {}\ndata: {}\n\n",
                    frame["type"].as_str().unwrap(),
                    frame
                )
                .as_bytes(),
            ),
        ));
    }
    out.push_str(&String::from_utf8_lossy(
        &t.finish(crate::codec::test_host::HOST),
    ));

    assert!(
        out.contains("functionCall") && out.contains("get_weather"),
        "the truncated tool call must still surface as a functionCall part on gemini ingress; got\n{out}"
    );
}

/// Every `IrBlockMeta` variant, with a witness value. The no-catch-all `match` is the point: a
fn all_block_metas() -> Vec<IrBlockMeta> {
    let witnesses = vec![
        IrBlockMeta::Text,
        IrBlockMeta::Thinking { kind: None },
        IrBlockMeta::RedactedThinking,
        IrBlockMeta::Image,
        IrBlockMeta::ToolUse {
            id: "toolu_x".into(),
            name: "f".into(),
        },
    ];
    for w in &witnesses {
        match w {
            IrBlockMeta::Text
            | IrBlockMeta::Thinking { .. }
            | IrBlockMeta::RedactedThinking
            | IrBlockMeta::Image
            | IrBlockMeta::ToolUse { .. } => {}
        }
    }
    witnesses
}

/// INV-B, exhaustively: for every registered protocol's writer and every `IrBlockMeta` variant, a
/// suppressed `BlockStart` (writer returns `None`) must be followed by a suppressed `BlockStop`
/// (also `None`) — otherwise a client receives a close for a block it never saw opened (7.2's
/// class) — UNLESS the block opens IMPLICITLY on the wire, in which case the start frame is
/// legitimately absent but the close is still mandatory. There are three declared exceptions where
/// a suppressed start pairs with a REQUIRED stop:
///   1. gemini ToolUse: `gemini/writer.rs`'s `BlockStart` returns `None` for ToolUse DELIBERATELY
///      (it buffers `(name, args)` and flushes the whole native `functionCall` part on `BlockStop`),
///      so `Some` on the stop is correct — suppressing it would silently delete the tool call.
///   2. bedrock Text and 3. bedrock Thinking: the AWS ConverseStream `ContentBlockStart$start`
///      union models only `toolUse`, so a text/reasoning block has NO `contentBlockStart` — it opens
///      IMPLICITLY on its first `contentBlockDelta` — but MUST still close with `contentBlockStop`
///      (a real ConverseStream sends the stop for every block). Suppressing the stop would drop the
///      block terminator a native AWS SDK keys its per-block reassembly off.
///
/// A FRESH writer is constructed for EVERY (name, meta) row via `protocol_for(name)` INSIDE the
/// loop body, not hoisted: cohere/responses/gemini writers carry per-stream `Mutex` state, so
/// reusing one writer across rows would make a later `None`/`None` row a statement about the
/// PREVIOUS row's writer having cleaned up after itself, not a property of a virgin stream.
///
/// The two protocol-axis assertions below are GATES, not the property under test here — each is
/// independently provable-to-fail: deleting the `PROTO_COHERE` arm from
/// `protocol_for` trips the first; appending a 7th name to `KNOWN_PROTOCOLS` trips the second. A
/// naive `assert_eq!(rows.len(), KNOWN_PROTOCOLS.len())` would be a TAUTOLOGY (both sides move
/// together, since rows are built BY iterating `KNOWN_PROTOCOLS`) and is deliberately not used.
#[test]
fn every_writer_that_suppresses_a_block_start_suppresses_its_stop() {
    assert_eq!(
        known_protocols().len(),
        6,
        "a 7th protocol was added — extend this table's expectations, then bump this count"
    );

    for &name in known_protocols() {
        assert!(
            protocol_for(name).is_some(),
            "known_protocols() lists {name} but it declares no codec"
        );

        for meta in all_block_metas() {
            let protocol = protocol_for(name).expect("checked above");
            let writer = protocol.writer();

            let start_index = 0usize;
            let start_emitted = writer
                .write_response_event(
                    crate::codec::test_host::HOST,
                    &IrStreamEvent::BlockStart {
                        index: start_index,
                        block: meta.clone(),
                        refusal: false,
                    },
                )
                .is_some();
            let stop_emitted = writer
                .write_response_event(
                    crate::codec::test_host::HOST,
                    &IrStreamEvent::BlockStop { index: start_index },
                )
                .is_some();

            let is_declared_exception = (name == PROTO_GEMINI
                && matches!(meta, IrBlockMeta::ToolUse { .. }))
                || (name == PROTO_BEDROCK
                    && matches!(
                        meta,
                        IrBlockMeta::Text
                            | IrBlockMeta::Thinking { .. }
                            | IrBlockMeta::RedactedThinking
                    ));

            if is_declared_exception {
                assert!(
                    !start_emitted && stop_emitted,
                    "{name:?}/{meta:?} is a declared exception: BlockStart must stay None (implicit \
                     open / buffered) and BlockStop must stay Some (the mandatory close / flush \
                     point); got start={start_emitted} stop={stop_emitted}"
                );
            } else if !start_emitted {
                assert!(
                    !stop_emitted,
                    "writer {name:?} suppressed BlockStart for {meta:?} but emitted a BlockStop \
                     for the same index — orphan close for a block never opened"
                );
            }
        }
    }
}

/// The deferred `redacted_thinking` start and its `content_block_stop` are decided by the SAME
#[test]
fn anthropic_deferred_redacted_start_pairs_with_its_stop_exactly_once() {
    let protocol = protocol_for(PROTO_ANTHROPIC).expect("anthropic protocol");
    let writer = protocol.writer();

    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: 0,
                    block: IrBlockMeta::RedactedThinking,
                    refusal: false,
                }
            )
            .is_none(),
        "the redacted BlockStart is deferred: it writes no frame"
    );
    let start = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::RedactedReasoningDelta(
                    "opaque-bytes".to_string(),
                ),
            },
        )
        .expect("the delta writes the block's sole content_block_start");
    assert_eq!(start.0, "content_block_start", "start event: {start:?}");
    assert_eq!(
        start.1["content_block"]["type"], "redacted_thinking",
        "the deferred start carries the redacted block: {:?}",
        start.1
    );
    assert!(
        writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: crate::codec::ir::IrDelta::RedactedReasoningDelta(
                        "more-bytes".to_string()
                    ),
                }
            )
            .is_none(),
        "a second redacted delta must not write a duplicate, unpaired content_block_start"
    );
    let stop = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("the written start owes its content_block_stop");
    assert_eq!(stop.0, "content_block_stop", "stop event: {stop:?}");
}

/// An Anthropic `content_block_start{type:"image"}` streamed block has no Bedrock ConverseStream
#[test]
fn anthropic_image_stream_to_bedrock_ingress_has_no_orphan_content_block_stop() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_img\",\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"aGk=\"}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            raw.extend(t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
        }
    t.finish(crate::codec::test_host::HOST);

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(
        buf.is_empty(),
        "all emitted frames must decode cleanly; {} bytes left",
        buf.len()
    );

    let started: std::collections::BTreeSet<u64> = frames
        .iter()
        .filter(|(et, _)| et == "contentBlockStart")
        .filter_map(|(_, payload)| serde_json::from_slice::<serde_json::Value>(payload).ok())
        .filter_map(|v| v.get("contentBlockIndex").and_then(|i| i.as_u64()))
        .collect();
    for (et, payload) in frames.iter().filter(|(et, _)| et == "contentBlockStop") {
        let v: serde_json::Value = serde_json::from_slice(payload).expect("valid JSON payload");
        let idx = v
            .get("contentBlockIndex")
            .and_then(|i| i.as_u64())
            .expect("contentBlockStop carries contentBlockIndex");
        assert!(
            started.contains(&idx),
            "orphan {et} for index {idx} with no preceding contentBlockStart: {frames:?}",
            frames = frames
                .iter()
                .map(|(t, p)| (t.clone(), String::from_utf8_lossy(p).to_string()))
                .collect::<Vec<_>>()
        );
    }
}

/// HIGH/conformance regression: a mid-stream upstream ERROR on a Bedrock-INGRESS cross-protocol
#[test]
fn test_translate_error_to_bedrock_ingress_is_exception_frame() {
    let mut t = StreamTranslate::new("bedrock", "anthropic").expect("bedrock ingress translator");

    let err_frame = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream is overloaded\"}}\n\n";
    let raw = t.feed(crate::codec::test_host::HOST, err_frame.as_bytes());
    assert!(!raw.is_empty(), "an error event must emit a frame");

    assert!(
        !raw.starts_with(b"event:") && !raw.starts_with(b"data:"),
        "bedrock ingress error must be a binary frame, not SSE text"
    );

    let headers_len = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let headers = String::from_utf8_lossy(&raw[12..12 + headers_len]);
    assert!(
        headers.contains(":message-type"),
        "frame carries a :message-type header; headers: {headers}"
    );
    assert!(
        headers.contains("exception"),
        ":message-type must be `exception`, not `event`; headers: {headers}"
    );
    assert!(
        headers.contains(":exception-type"),
        "frame carries an :exception-type header; headers: {headers}"
    );

    assert!(
        headers.contains("serviceUnavailableException"),
        ":exception-type names the transient overload Converse exception; headers: {headers}"
    );

    let total_len = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    assert_eq!(total_len, raw.len(), "total_len matches the bytes emitted");

    let payload = &raw[12 + headers_len..total_len - 4];
    let v: serde_json::Value = serde_json::from_slice(payload).expect("valid JSON payload");
    assert!(
        v.get("message").and_then(|m| m.as_str()).is_some(),
        "exception frame carries a JSON message body; got {v}"
    );
}

/// Conformance regression: on a cross-protocol Gemini-INGRESS stream, the MessageStart
#[test]
fn test_translate_to_gemini_ingress_synthesizes_response_id() {
    let mut t = StreamTranslate::new("gemini", "openai").expect("gemini ingress translator");

    let chunk = "data: {\"id\":\"chatcmpl-abc\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
    let out = String::from_utf8(t.feed(crate::codec::test_host::HOST, chunk.as_bytes())).unwrap();
    assert!(
        out.contains("responseId"),
        "gemini cross-protocol stream must carry a synthesized responseId; got:\n{out}"
    );

    assert!(
        !out.contains("chatcmpl-abc"),
        "foreign backend id must be stripped, not leaked; got:\n{out}"
    );
}

/// Regression: a CROSS-PROTOCOL tool call streamed to a Gemini client
#[test]
fn test_translate_to_gemini_tool_call_single_functioncall_part() {
    let mut t = StreamTranslate::new("gemini", "openai").expect("gemini ingress translator");
    let mut out = String::new();
    for frame in [

            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",

            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\"}}]}}]}\n\n",

            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}}]}\n\n",

            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());

    let payloads = data_payloads(&out);
    let func_parts: Vec<&serde_json::Value> = payloads
        .iter()
        .filter_map(|p| {
            p.pointer("/candidates/0/content/parts")
                .and_then(|parts| parts.as_array())
        })
        .flatten()
        .filter_map(|part| part.get("functionCall"))
        .collect();

    assert_eq!(
            func_parts.len(),
            1,
            "exactly one native functionCall part expected (no empty-args-then-args split); got:\n{out}"
        );
    let func = func_parts[0];
    assert_eq!(
        func.pointer("/name").and_then(|n| n.as_str()),
        Some("get_weather"),
        "the single functionCall part must carry the name; got:\n{out}"
    );
    assert_eq!(
        func.pointer("/args/city").and_then(|c| c.as_str()),
        Some("SF"),
        "the single functionCall part must carry the args; got:\n{out}"
    );

    assert!(
        !func_parts.iter().any(|f| f
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .is_empty()),
        "no nameless functionCall part may be emitted; got:\n{out}"
    );
}

/// A CRLF-delimited SSE upstream (`\r\n\r\n` frame terminators — spec-legal, emitted by some
#[test]
fn test_translate_crlf_sse_frames() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");

    let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\r\n\r\n";
    let out = String::from_utf8(t.feed(crate::codec::test_host::HOST, chunk.as_bytes())).unwrap();
    assert!(
        !out.is_empty(),
        "CRLF SSE must produce translated output, not stall"
    );
    assert!(
        out.contains("He") && out.contains("llo"),
        "both CRLF-delimited deltas must translate; got:\n{out}"
    );
    assert!(!t.aborted, "CRLF stream must not be abandoned");
}

/// Decoder also works when the binary frames arrive split across feed() calls (partial frame
#[test]
fn test_translate_bedrock_eventstream_split_chunks() {
    let mut st = StreamTranslate::new("anthropic", "bedrock").expect("translator");
    let mut bytes = es_frame("messageStart", br#"{"role":"assistant"}"#);
    bytes.extend(es_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"text":"Yo"}}"#,
    ));
    let split = bytes.len() - 6;
    let mut out = st.feed(crate::codec::test_host::HOST, &bytes[..split]);
    out.extend(st.feed(crate::codec::test_host::HOST, &bytes[split..]));
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains("Yo"),
        "text survives a frame split across chunks; got:\n{s}"
    );
}

/// Collect the JSON payloads of all `data:` lines (excluding `[DONE]`).
fn data_payloads(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|s| s.trim())
        .filter(|s| *s != "[DONE]")
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}

#[test]
fn test_translate_anthropic_egress_to_openai_ingress() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let mut out = String::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());

    assert!(
        !out.contains("event:"),
        "OpenAI output must have no event: lines; got {out}"
    );
    let payloads = data_payloads(&out);
    assert!(
        payloads.iter().any(|p| p
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
            == Some("hi")),
        "translated content 'hi' missing; got {out}"
    );
    assert!(
        payloads.iter().any(|p| p
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            == Some("stop")),
        "finish_reason 'stop' missing; got {out}"
    );
    assert!(
        out.trim_end().ends_with("data: [DONE]"),
        "OpenAI stream must end with data: [DONE]; got {out}"
    );
}

#[test]
fn test_translate_openai_egress_to_anthropic_ingress() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");
    let mut out = String::new();
    for frame in [
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }

    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());
    assert!(
        !out.contains("[DONE]"),
        "Anthropic ingress must not emit a [DONE] terminator; got {out}"
    );
    assert!(
        out.contains("event: message_start"),
        "missing message_start; got {out}"
    );
    assert!(
        out.contains("event: content_block_delta"),
        "missing content_block_delta; got {out}"
    );
    assert!(
        out.contains("text_delta") && out.contains("hi"),
        "missing text_delta 'hi'; got {out}"
    );
    assert!(
        out.contains("event: message_stop"),
        "missing message_stop; got {out}"
    );
}

#[test]
fn test_translate_anthropic_egress_to_gemini_ingress_preserves_input_tokens() {
    let mut t = StreamTranslate::new("gemini", "anthropic").expect("gemini ingress translator");
    let mut out = String::new();
    for frame in [

            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",

            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());
    let payloads = data_payloads(&out);

    let usage = payloads
        .iter()
        .find_map(|p| p.pointer("/usageMetadata"))
        .unwrap_or_else(|| panic!("no usageMetadata in translated stream; got {out}"));
    assert_eq!(
        usage.get("promptTokenCount").and_then(|v| v.as_u64()),
        Some(42),
        "input tokens captured at message_start must survive to the terminal usage; got {out}"
    );
    assert_eq!(
        usage.get("candidatesTokenCount").and_then(|v| v.as_u64()),
        Some(7),
        "output tokens from message_delta must be reported; got {out}"
    );
}

#[test]
fn test_anthropic_stream_usage_roundtrips_input_and_output_tokens() {
    let reader = AnthropicReader;
    let writer = AnthropicWriter;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let start_in = serde_json::json!({
        "type":"message_start",
        "message":{"role":"assistant","usage":{"input_tokens":42,"output_tokens":0}}
    });
    let start_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "message_start",
        &start_in,
        &mut state,
    );
    let (_et, start_out) = writer
        .write_response_event(crate::codec::test_host::HOST, &start_evs[0])
        .expect("message_start writes");
    assert_eq!(
        start_out
            .pointer("/message/usage/input_tokens")
            .and_then(|v| v.as_u64()),
        Some(42),
        "message_start must round-trip input_tokens"
    );

    let delta_in = serde_json::json!({
        "type":"message_delta",
        "delta":{"stop_reason":"end_turn"},
        "usage":{"output_tokens":7}
    });
    let delta_evs = reader.read_response_events(
        crate::codec::test_host::HOST,
        "message_delta",
        &delta_in,
        &mut state,
    );
    let (_et, delta_out) = writer
        .write_response_event(crate::codec::test_host::HOST, &delta_evs[0])
        .expect("message_delta writes");
    assert_eq!(
        delta_out
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_u64()),
        Some(7),
        "message_delta must round-trip output_tokens"
    );
}

#[test]
fn test_translate_anthropic_egress_to_cohere_ingress() {
    let mut t = StreamTranslate::new("cohere", "anthropic").expect("cohere ingress translator");
    let mut out = String::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());

    assert!(
        !out.contains("event:"),
        "Cohere output must have no event: lines; got {out}"
    );

    assert!(
        !out.contains("[DONE]"),
        "Cohere stream must NOT emit a [DONE] sentinel; got {out}"
    );
    let payloads = data_payloads(&out);

    assert!(
        payloads.iter().any(|p| p["type"] == "content-delta"
            && p.pointer("/delta/message/content/text")
                .and_then(|v| v.as_str())
                == Some("hi")),
        "missing cohere content-delta carrying 'hi'; got {out}"
    );

    assert!(
        payloads.iter().any(|p| p["type"] == "message-end"
            && p.pointer("/delta/finish_reason").and_then(|v| v.as_str()) == Some("COMPLETE")),
        "missing cohere message-end COMPLETE; got {out}"
    );
}

#[test]
fn test_translate_anthropic_egress_to_responses_ingress() {
    let mut t =
        StreamTranslate::new("responses", "anthropic").expect("responses ingress translator");
    let mut out = String::new();
    for frame in [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    out.push_str(&String::from_utf8(t.finish(crate::codec::test_host::HOST)).unwrap());

    assert!(
        out.contains("event: response.created"),
        "missing Responses event: response.created; got {out}"
    );
    assert!(
        out.contains("event: response.completed"),
        "missing Responses event: response.completed; got {out}"
    );

    assert!(
        !out.contains("[DONE]"),
        "Responses stream must NOT emit a [DONE] sentinel; got {out}"
    );
}

/// A mid-stream terminal error must be framed by the writer that has been driving THIS stream, not
#[test]
fn responses_ingress_terminal_error_continues_the_live_stream_identity() {
    let mut t =
        StreamTranslate::new("responses", "anthropic").expect("responses ingress translator");
    let mut out = String::new();
    for frame in [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"id\":\"msg_live\",\"model\":\"m\"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    ] {
        out.push_str(&String::from_utf8_lossy(&t.feed(crate::codec::test_host::HOST, frame.as_bytes())));
    }
    let payloads = data_payloads(&out);
    let last_seq = payloads
        .iter()
        .filter_map(|p| p.get("sequence_number").and_then(|s| s.as_u64()))
        .max()
        .expect("the frames already sent carry sequence numbers");
    let created_id = payloads
        .iter()
        .find(|p| p.get("type").and_then(|v| v.as_str()) == Some("response.created"))
        .and_then(|p| p.pointer("/response/id"))
        .and_then(|v| v.as_str())
        .expect("response.created carries the stream's response id")
        .to_string();

    let err = IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("The response stream was interrupted.".to_string()),
        retry_after: None,
    };
    let (event_type, data) = t
        .terminal_error_frame(crate::codec::test_host::HOST, &err)
        .expect("a Responses ingress frames its terminal error in band");

    assert_eq!(
        event_type, "response.failed",
        "the terminal error rides the Responses failure event; got {event_type}"
    );
    let seq = data
        .get("sequence_number")
        .and_then(|s| s.as_u64())
        .expect("the failure event carries a sequence number");
    assert!(
        seq > last_seq,
        "the failure event must CONTINUE the stream's sequence (last {last_seq}), got {seq}: {data}"
    );
    assert_eq!(
        data.pointer("/response/id").and_then(|v| v.as_str()),
        Some(created_id.as_str()),
        "the failure event must carry the id the stream opened on: {data}"
    );
}

#[test]
fn test_translate_openai_include_usage_egress_to_bedrock_ingress_single_metadata() {
    let mut t = StreamTranslate::new("bedrock", "openai").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":11}}\n\n",
        "data: [DONE]\n\n",
    ] {
        raw.extend_from_slice(&t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
    }
    raw.extend_from_slice(&t.finish(crate::codec::test_host::HOST));

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(buf.is_empty(), "all frames must decode cleanly");

    let metadata: Vec<&(String, Vec<u8>)> =
        frames.iter().filter(|(et, _)| et == "metadata").collect();
    assert_eq!(
        metadata.len(),
        1,
        "a native ConverseStream emits exactly ONE metadata frame; got {}",
        metadata.len()
    );
    let md: serde_json::Value =
        serde_json::from_slice(&metadata[0].1).expect("metadata payload is JSON");
    assert_eq!(
        md["usage"]["inputTokens"], 7,
        "metadata must carry the REAL input tokens, not a zero frame; got {md}"
    );
    assert_eq!(
        md["usage"]["outputTokens"], 11,
        "metadata must carry the REAL output tokens; got {md}"
    );

    let stops = frames.iter().filter(|(et, _)| et == "messageStop").count();
    assert_eq!(stops, 1, "exactly one messageStop frame");
}

#[test]
fn test_translate_openai_include_usage_to_anthropic_ingress_no_post_stop_message_delta() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("anthropic ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":11}}\n\n",
        "data: [DONE]\n\n",
    ] {
        raw.extend_from_slice(&t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
    }
    raw.extend_from_slice(&t.finish(crate::codec::test_host::HOST));
    let wire = String::from_utf8_lossy(&raw);

    assert!(
        wire.contains("\"input_tokens\":7,\"output_tokens\":11"),
        "the folded terminal message_delta must carry the merged usage (7 in / 11 out); wire:\n{wire}"
    );

    let stop_at = wire
        .rfind("event: message_stop")
        .or_else(|| wire.rfind("\"type\":\"message_stop\""))
        .or_else(|| wire.rfind("message_stop"))
        .expect("anthropic stream must emit a message_stop");
    let after = &wire[stop_at..];
    assert!(
        !after.contains("message_delta"),
        "no message_delta may follow message_stop on the wire; tail after stop:\n{after}"
    );
}

#[test]
fn test_translate_openai_no_include_usage_egress_to_bedrock_ingress_emits_metadata() {
    let mut t = StreamTranslate::new("bedrock", "openai").expect("bedrock ingress translator");
    let mut raw: Vec<u8> = Vec::new();
    for frame in [
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ] {
        raw.extend_from_slice(&t.feed(crate::codec::test_host::HOST, frame.as_bytes()));
    }

    raw.extend_from_slice(&t.finish(crate::codec::test_host::HOST));

    let mut buf = raw.clone();
    let frames = crate::codec::eventstream::drain_frames(&mut buf);
    assert!(buf.is_empty(), "all frames must decode cleanly");

    let metadata: Vec<&(String, Vec<u8>)> =
        frames.iter().filter(|(et, _)| et == "metadata").collect();
    assert_eq!(
        metadata.len(),
        1,
        "default OpenAI stream (no include_usage) must STILL terminate with exactly one \
             metadata frame; got {} frames: {:?}",
        metadata.len(),
        frames.iter().map(|(et, _)| et.as_str()).collect::<Vec<_>>()
    );

    let md: serde_json::Value =
        serde_json::from_slice(&metadata[0].1).expect("metadata payload is JSON");
    assert_eq!(md["usage"]["inputTokens"], 0);
    assert_eq!(md["usage"]["outputTokens"], 0);

    let types: Vec<&str> = frames.iter().map(|(et, _)| et.as_str()).collect();
    let stop_pos = types.iter().position(|t| *t == "messageStop");
    let meta_pos = types.iter().position(|t| *t == "metadata");
    assert!(
        stop_pos.is_some() && meta_pos.is_some() && stop_pos < meta_pos,
        "messageStop must precede metadata (native order); got {types:?}"
    );

    assert_eq!(
        frames.iter().filter(|(et, _)| et == "messageStop").count(),
        1,
        "exactly one messageStop frame"
    );
}

#[test]
fn test_translate_split_frame_reassembly() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
    let (a, b) = frame.as_bytes().split_at(20);
    assert!(
        t.feed(crate::codec::test_host::HOST, a).is_empty(),
        "partial frame must yield no output"
    );
    let s = String::from_utf8(t.feed(crate::codec::test_host::HOST, b)).unwrap();
    assert!(
        s.contains("\"content\":\"hi\""),
        "completed frame must translate to openai content; got {s}"
    );
}

#[test]
fn test_translate_tool_call_fidelity() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");
    let mut out = String::new();
    for frame in [
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\\\":\\\"SF\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ] {
            out.push_str(&String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap());
        }
    assert!(
        out.contains("content_block_start"),
        "missing content_block_start; got {out}"
    );
    assert!(
        out.contains("tool_use"),
        "tool_use block type missing; got {out}"
    );

    assert!(
        out.contains("get_weather"),
        "tool name must survive; got {out}"
    );
    assert!(
        !out.contains("call_1"),
        "foreign `call_1` id must NOT leak to the Anthropic client; got {out}"
    );

    let emitted = data_payloads(&out)
        .into_iter()
        .find_map(|p| {
            p.pointer("/content_block/id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .expect("a content_block_start carrying a tool_use id");
    assert!(
        emitted.starts_with("toolu_"),
        "emitted tool id must be Anthropic-native; got {emitted}"
    );
    assert_eq!(
        decode_native_tool_id("anthropic", &emitted).as_deref(),
        Some("call_1"),
        "the reshaped id must decode back to the original egress `call_1`; got {emitted}"
    );
    assert!(
        out.contains("input_json_delta"),
        "missing input_json_delta; got {out}"
    );
}

#[test]
fn test_translate_same_protocol_is_none() {
    assert!(StreamTranslate::new("openai", "openai").is_none());
    assert!(StreamTranslate::new("anthropic", "anthropic").is_none());
}

/// Drain `n` complete SSE frames through one `feed` and return (frames translated, time in the
#[cfg(test)]
fn drain_frames_once(n: usize) -> (usize, std::time::Duration) {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let mut blob = String::with_capacity(n * 96);
    for _ in 0..n {
        blob.push_str(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            );
    }
    let start = std::time::Instant::now();
    let out =
        String::from_utf8(t.feed(crate::codec::test_host::HOST, blob.as_bytes())).expect("utf8");
    let elapsed = start.elapsed();
    (out.matches("\"content\":\"x\"").count(), elapsed)
}

#[test]
fn test_translate_many_frames_in_one_feed_is_linear_and_complete() {
    const N: usize = 20_000;
    const REPS: usize = 3;

    const MAX_GROWTH: f64 = 3.0;

    let mut best_ratio = f64::MAX;
    let mut best_n = std::time::Duration::MAX;
    let mut best_2n = std::time::Duration::MAX;
    for _ in 0..REPS {
        let (count_n, t_n) = drain_frames_once(N);
        assert_eq!(count_n, N, "all {N} frames must translate exactly once");

        let (count_2n, t_2n) = drain_frames_once(2 * N);
        assert_eq!(
            count_2n,
            2 * N,
            "all {} frames must translate exactly once",
            2 * N
        );

        let ratio = t_2n.as_secs_f64() / t_n.as_secs_f64();
        if ratio < best_ratio {
            best_ratio = ratio;

            best_n = t_n;
            best_2n = t_2n;
        }
    }

    assert!(
        best_n >= std::time::Duration::from_micros(500),
        "the N={N} drain completed in {best_n:?}, too fast to time reliably, so the growth ratio \
         below would be measuring clock noise rather than the translator. Raise N."
    );

    assert!(
        best_ratio <= MAX_GROWTH,
        "doubling the frame count multiplied the drain time by {best_ratio:.2}x in the best of \
         {REPS} paired runs (N={N} took {best_n:?}, 2N took {best_2n:?} in that same run). Linear \
         reassembly doubles (about 2x); quadratic quadruples (about 4x). Above {MAX_GROWTH:.1}x \
         means the per-frame front-draining regression is back."
    );
}

#[test]
fn test_translate_frames_split_across_chunks_reassemble() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");
    let mut blob = String::new();
    for i in 0..50 {
        blob.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{i}\"}}}}]}}\n\n"
        ));
    }

    let mut out = String::new();
    let bytes = blob.as_bytes();
    let mut p = 0;
    while p < bytes.len() {
        let end = (p + 7).min(bytes.len());
        out.push_str(
            &String::from_utf8(t.feed(crate::codec::test_host::HOST, &bytes[p..end])).unwrap(),
        );
        p = end;
    }
    for i in 0..50 {
        assert!(
            out.contains(&format!("\"text\":\"t{i}\"")),
            "frame t{i} must survive chunk-boundary reassembly; got {out}"
        );
    }
}

/// Multiple `data:` lines in one SSE frame must be concatenated with `\n` (SSE spec),
#[test]
fn test_parse_sse_frame_concatenates_multiple_data_lines() {
    let frame = b"event: e\ndata: {\"a\":1,\ndata: \"b\":2}\n\n";
    let (et, data) = parse_sse_frame(frame).expect("frame has data");
    assert_eq!(et, "e");
    assert_eq!(data, "{\"a\":1,\n\"b\":2}");

    let v: serde_json::Value = serde_json::from_str(&data).expect("joined data parses");
    assert_eq!(v.get("a"), Some(&serde_json::json!(1)));
    assert_eq!(v.get("b"), Some(&serde_json::json!(2)));
}

/// A frame carrying only an `event:` line (no `data:`) must return None.
#[test]
fn test_parse_sse_frame_event_only_is_none() {
    assert!(parse_sse_frame(b"event: ping\n\n").is_none());
    assert!(parse_sse_frame(b"\n\n").is_none());
}

/// A `data:` line with empty value still yields Some (caller treats empty payload as a
#[test]
fn test_parse_sse_frame_done_sentinel() {
    let (et, data) = parse_sse_frame(b"data: [DONE]\n\n").expect("data line present");
    assert_eq!(et, "");
    assert_eq!(data, "[DONE]");
}

/// An upstream that splits a single JSON event across two `data:` lines must still translate
#[test]
fn test_translate_multiline_data_payload() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\ndata: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
    let s = String::from_utf8(t.feed(crate::codec::test_host::HOST, frame.as_bytes())).unwrap();
    assert!(
        s.contains("\"content\":\"hi\""),
        "multi-line data payload must reassemble and translate; got {s}"
    );
}

/// An upstream that streams bytes without ever emitting a frame terminator must not grow the
#[test]
fn test_feed_aborts_on_unbounded_buffer() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let chunk = vec![b'x'; 1024 * 1024];
    let mut total = 0usize;

    for _ in 0..18 {
        let out = t.feed(crate::codec::test_host::HOST, &chunk);
        assert!(out.is_empty(), "garbage stream must produce no output");
        total += chunk.len();
        if t.aborted {
            break;
        }
        assert!(
            t.buf.len() <= StreamTranslate::MAX_BUF,
            "buffer must stay within MAX_BUF while accumulating"
        );
    }
    assert!(
        t.aborted,
        "stream must abort after exceeding MAX_BUF (fed {total} bytes)"
    );
    assert!(t.buf.is_empty(), "aborted stream must release its buffer");

    assert!(
        t.feed(crate::codec::test_host::HOST, b"data: {\"choices\":[]}\n\n")
            .is_empty(),
        "feeds after abort must be ignored"
    );
}

/// Conformance (`StreamTranslate::abort` / `finish` for bedrock ingress): when the SSE
#[test]
fn test_bedrock_ingress_overflow_abort_emits_exception_frame() {
    let mut t = StreamTranslate::new("bedrock", "openai").expect("translator");
    assert!(t.ingress_eventstream, "bedrock ingress must be eventstream");
    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..18 {
        let _ = t.feed(crate::codec::test_host::HOST, &chunk);
        if t.aborted {
            break;
        }
    }
    assert!(t.aborted, "stream must abort after exceeding MAX_BUF");

    let mut tail = t.finish(crate::codec::test_host::HOST);
    assert!(
        !tail.is_empty(),
        "aborted bedrock-ingress finish must emit a terminal exception frame, not a bare close"
    );
    let frames = crate::codec::eventstream::drain_frames(&mut tail);
    let names: Vec<&str> = frames.iter().map(|(ty, _)| ty.as_str()).collect();
    assert_eq!(
            names.as_slice(),
            ["internalServerException"],
            "aborted bedrock-ingress stream must terminate with a single modeled exception frame; got {names:?}"
        );
}

/// Regression: an SSE-INGRESS stream aborted by reassembly-buffer overflow must NOT
#[test]
fn test_sse_ingress_overflow_abort_emits_native_error_frame() {
    let chunk = vec![b'x'; 1024 * 1024];

    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");
    assert!(
        !t.ingress_eventstream,
        "anthropic ingress must be SSE, not eventstream"
    );
    for _ in 0..18 {
        let _ = t.feed(crate::codec::test_host::HOST, &chunk);
        if t.aborted {
            break;
        }
    }
    assert!(
        t.aborted,
        "anthropic-ingress stream must abort after MAX_BUF"
    );
    let tail = String::from_utf8(t.finish(crate::codec::test_host::HOST))
        .expect("SSE error tail is UTF-8");
    assert!(
        !tail.is_empty(),
        "aborted SSE-ingress finish must emit a native error frame, not a silent bare close"
    );
    assert!(
        tail.starts_with("event: error\n"),
        "anthropic-ingress abort must emit a native `event: error` frame; got {tail:?}"
    );

    let data_line = tail
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .map(|s| s.trim())
        .expect("error frame carries a data: line");
    let v: serde_json::Value = serde_json::from_str(data_line).expect("error frame data: is JSON");
    assert_eq!(
        v.get("type").and_then(|x| x.as_str()),
        Some("error"),
        "anthropic stream error payload is `type:error`; got {v}"
    );
    assert!(
        v.get("error").and_then(|e| e.get("message")).is_some(),
        "anthropic stream error carries an error.message; got {v}"
    );

    let mut t2 = StreamTranslate::new("openai", "anthropic").expect("translator");
    for _ in 0..18 {
        let _ = t2.feed(crate::codec::test_host::HOST, &chunk);
        if t2.aborted {
            break;
        }
    }
    assert!(t2.aborted);
    let tail2 = String::from_utf8(t2.finish(crate::codec::test_host::HOST))
        .expect("SSE error tail is UTF-8");
    assert!(
        tail2.contains("\"error\""),
        "openai-ingress abort must emit a native bare `data:` error envelope; got {tail2:?}"
    );
    assert!(
        tail2.trim_end().ends_with("data: [DONE]"),
        "openai-ingress aborted stream must still terminate with `data: [DONE]`; got {tail2:?}"
    );

    let err_pos = tail2.find("\"error\"").expect("error envelope present");
    let done_pos = tail2.find("data: [DONE]").expect("[DONE] present");
    assert!(
        err_pos < done_pos,
        "the error frame must precede [DONE]; got {tail2:?}"
    );
}

/// The scanned-offset optimization must not break terminator detection when `\n\n` straddles a
#[test]
fn test_feed_terminator_straddles_chunk_boundary() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n";

    assert!(
        t.feed(crate::codec::test_host::HOST, frame.as_bytes())
            .is_empty(),
        "no terminator yet"
    );
    let s = String::from_utf8(t.feed(crate::codec::test_host::HOST, b"\n")).unwrap();
    assert!(
        s.contains("\"content\":\"hi\""),
        "terminator split across chunks must still complete the frame; got {s}"
    );
}

/// Many tiny chunks comprising a single large frame must reassemble and translate exactly once.
#[test]
fn test_feed_large_frame_many_chunks() {
    let mut t = StreamTranslate::new("openai", "anthropic").expect("translator");
    let big = "x".repeat(200_000);
    let frame = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{big}\"}}}}\n\n"
        );
    let bytes = frame.as_bytes();
    let mut out = Vec::new();
    for chunk in bytes.chunks(64) {
        out.extend(t.feed(crate::codec::test_host::HOST, chunk));
    }
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains(&big),
        "large frame split across many chunks must reassemble"
    );
}

#[test]
fn test_anthropic_read_response_decode() {
    let data = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 3,
            "cache_creation_input_tokens": null,
            "cache_read_input_tokens": null
        }
    });

    let reader = AnthropicReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &data)
        .expect("should parse");

    assert_eq!(resp.role, crate::codec::ir::IrRole::Assistant);
    assert_eq!(resp.content.len(), 1);
    if let crate::codec::ir::IrBlock::Text { text, .. } = &resp.content[0] {
        assert_eq!(text, "hi");
    } else {
        panic!("expected Text block");
    }
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
    assert_eq!(resp.usage.input_tokens, 5);
}

#[test]
fn test_openai_read_response_decode() {
    let data = serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3
        }
    });

    let reader = OpenAiReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &data)
        .expect("should parse");

    assert_eq!(resp.role, crate::codec::ir::IrRole::Assistant);
    assert_eq!(resp.content.len(), 1);
    if let crate::codec::ir::IrBlock::Text { text, .. } = &resp.content[0] {
        assert_eq!(text, "hi");
    } else {
        panic!("expected Text block");
    }
    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn)
    );
    assert_eq!(resp.usage.input_tokens, 5);
}

#[test]
fn test_cross_protocol_openai_to_anthropic() {
    let openai_data = serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3
        }
    });

    let ir_resp = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &openai_data)
        .expect("OpenAI read");
    let anthropic_json = anthropic_writer().write_response(crate::codec::test_host::HOST, &ir_resp);

    assert_eq!(
        anthropic_json.get("type").and_then(|v| v.as_str()),
        Some("message")
    );
    if let Some(content_arr) = anthropic_json.get("content").and_then(|c| c.as_array()) {
        assert!(!content_arr.is_empty());
        let first_block = &content_arr[0];
        assert_eq!(
            first_block.get("type").and_then(|v| v.as_str()),
            Some("text")
        );
        assert_eq!(first_block.get("text").and_then(|v| v.as_str()), Some("hi"));
    } else {
        panic!("missing content array");
    }
    assert_eq!(
        anthropic_json.get("stop_reason").and_then(|v| v.as_str()),
        Some("end_turn")
    );
}

#[test]
fn test_cross_protocol_anthropic_to_openai() {
    let anthropic_data = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 3,
            "cache_creation_input_tokens": null,
            "cache_read_input_tokens": null
        }
    });

    let ir_resp = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &anthropic_data)
        .expect("Anthropic read");
    let openai_json = openai_writer().write_response(crate::codec::test_host::HOST, &ir_resp);

    assert_eq!(
        openai_json.get("object").and_then(|v| v.as_str()),
        Some("chat.completion")
    );
    if let Some(choices_arr) = openai_json.get("choices").and_then(|c| c.as_array()) {
        assert!(!choices_arr.is_empty());
        let choice = &choices_arr[0];
        if let Some(msg) = choice.get("message") {
            assert_eq!(msg.get("role").and_then(|v| v.as_str()), Some("assistant"));
            assert_eq!(msg.get("content").and_then(|v| v.as_str()), Some("hi"));
        } else {
            panic!("missing message");
        }
        assert_eq!(
            choice.get("finish_reason").and_then(|v| v.as_str()),
            Some("stop")
        );
    } else {
        panic!("missing choices array");
    }
}

#[test]
fn test_cross_protocol_tool_use_response() {
    let openai_data = serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{\"x\":1}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3
        }
    });

    let ir_resp = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &openai_data)
        .expect("OpenAI read");

    assert_eq!(ir_resp.content.len(), 1);
    if let crate::codec::ir::IrBlock::ToolUse {
        id, name, input, ..
    } = &ir_resp.content[0]
    {
        assert_eq!(id, "call_1");
        assert_eq!(name, "f");
        match input {
            serde_json::Value::Object(obj) => {
                assert_eq!(obj.get("x"), Some(&serde_json::json!(1)));
            }
            _ => panic!("input should be Object"),
        }
    } else {
        panic!("expected ToolUse block");
    }

    let anthropic_json = anthropic_writer().write_response(crate::codec::test_host::HOST, &ir_resp);

    if let Some(content_arr) = anthropic_json.get("content").and_then(|c| c.as_array()) {
        assert!(!content_arr.is_empty());
        let first_block = &content_arr[0];
        assert_eq!(
            first_block.get("type").and_then(|v| v.as_str()),
            Some("tool_use")
        );
        assert_eq!(
            first_block.get("id").and_then(|v| v.as_str()),
            Some("call_1")
        );
        assert_eq!(first_block.get("name").and_then(|v| v.as_str()), Some("f"));

        if let Some(input_val) = first_block.get("input") {
            match input_val {
                serde_json::Value::Object(obj) => {
                    assert_eq!(obj.get("x"), Some(&serde_json::json!(1)));
                }
                _ => panic!("input should be Object"),
            }
        } else {
            panic!("missing input");
        }
    } else {
        panic!("missing content array");
    }

    assert_eq!(
        anthropic_json.get("stop_reason").and_then(|v| v.as_str()),
        Some("tool_use")
    );
}

#[test]
fn test_tool_id_remap_reshapes_to_ingress_native_prefix() {
    let mut remap = ToolIdRemap::default();
    let native = remap.native_for("anthropic", "call_abc123");
    assert!(
        native.starts_with("toolu_"),
        "anthropic-ingress id must carry the native `toolu_` prefix, got {native}"
    );
    assert!(
        !native.contains("call_"),
        "the foreign `call_` shape must NOT survive into the client id, got {native}"
    );

    assert!(ToolIdRemap::default()
        .native_for("bedrock", "call_x")
        .starts_with("tooluse_"));
    assert!(ToolIdRemap::default()
        .native_for("openai", "toolu_y")
        .starts_with("call_"));

    assert_eq!(
        ToolIdRemap::default().native_for("gemini", "call_z"),
        "call_z"
    );
}

#[test]
fn test_tool_id_remap_is_a_stable_reversible_bijection() {
    let mut remap = ToolIdRemap::default();
    let a1 = remap.native_for("anthropic", "call_one");
    let a2 = remap.native_for("anthropic", "call_one");
    let b = remap.native_for("anthropic", "call_two");
    assert_eq!(a1, a2, "a repeated egress id must map stably");
    assert_ne!(a1, b, "distinct egress ids must map to distinct native ids");
    assert_eq!(
        decode_native_tool_id("anthropic", &a1).as_deref(),
        Some("call_one")
    );
    assert_eq!(
        decode_native_tool_id("anthropic", &b).as_deref(),
        Some("call_two")
    );

    assert_eq!(
        decode_native_tool_id("anthropic", "toolu_01RealClientId"),
        None
    );

    let foreign_shaped = format!("call_{TOOL_ID_REMAP_MARKER}{}", ::hex::encode("x"));
    assert_eq!(
        decode_native_tool_id("anthropic", &foreign_shaped),
        None,
        "a foreign-prefix busbar-shaped id must not be decoded on a non-matching ingress"
    );
    let bare_shaped = format!("{TOOL_ID_REMAP_MARKER}{}", ::hex::encode("y"));
    assert_eq!(
        decode_native_tool_id("anthropic", &bare_shaped),
        None,
        "a bare `bb1<hex>` (empty-prefix) id must not be decoded on a non-Cohere ingress"
    );

    let marker_only = format!("toolu_{TOOL_ID_REMAP_MARKER}");
    assert_eq!(
        decode_native_tool_id("anthropic", &marker_only),
        None,
        "a marker-only id (empty hex tail) must not decode to an empty string"
    );
    assert_eq!(
        decode_native_tool_id("anthropic", TOOL_ID_REMAP_MARKER),
        None,
        "a bare marker with no prefix and empty hex tail must pass through verbatim"
    );

    assert_eq!(
        decode_native_tool_id("openai", &foreign_shaped).as_deref(),
        Some("x")
    );
}

#[test]
fn test_tool_id_remap_response_round_trip_through_seam() {
    let mut ir = OpenAiReader
        .read_response(
            crate::codec::test_host::HOST,
            &serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_seam",
                            "type": "function",
                            "function": {"name": "f", "arguments": "{\"x\":1}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3}
            }),
        )
        .expect("OpenAI read");
    ToolIdRemap::default().remap_response("anthropic", &mut ir);
    let client_id = match &ir.content[0] {
        crate::codec::ir::IrBlock::ToolUse { id, .. } => id.clone(),
        other => panic!("expected ToolUse, got {other:?}"),
    };
    assert!(
        client_id.starts_with("toolu_") && !client_id.contains("call_seam"),
        "client must see a native id, not the foreign `call_seam`, got {client_id}"
    );

    let mut messages = vec![crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::Tool,
        content: vec![crate::codec::ir::IrBlock::ToolResult {
            tool_use_id: client_id,
            content: vec![],
            is_error: false,
            cache_control: None,
        }],
    }];
    decode_request_tool_ids("anthropic", &mut messages);
    match &messages[0].content[0] {
        crate::codec::ir::IrBlock::ToolResult { tool_use_id, .. } => {
            assert_eq!(
                tool_use_id, "call_seam",
                "the egress backend must see the id it originally issued"
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn test_tool_id_remap_event_reshapes_block_start() {
    let mut ev = IrStreamEvent::BlockStart {
        index: 0,
        block: IrBlockMeta::ToolUse {
            id: "call_stream".to_string(),
            name: "f".to_string(),
        },
        refusal: false,
    };
    ToolIdRemap::default().remap_event("anthropic", &mut ev);
    match ev {
        IrStreamEvent::BlockStart {
            block: IrBlockMeta::ToolUse { id, .. },
            ..
        } => {
            assert!(id.starts_with("toolu_") && id != "call_stream");
            assert_eq!(
                decode_native_tool_id("anthropic", &id).as_deref(),
                Some("call_stream")
            );
        }
        other => panic!("expected BlockStart ToolUse, got {other:?}"),
    }
}

#[test]
fn test_same_protocol_roundtrip_idempotence() {
    let original_data = serde_json::json!({
        "id": "msg_01TestRoundtripIdempotence",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "tool_1", "name": "get_weather", "input": {"loc": "SF"}}
        ],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
            "service_tier": "standard"
        }
    });

    let reader = AnthropicReader;
    let writer = AnthropicWriter;

    let ir1 = reader
        .read_response(crate::codec::test_host::HOST, &original_data)
        .expect("first read");

    let written_json = writer.write_response(crate::codec::test_host::HOST, &ir1);

    let ir2 = reader
        .read_response(crate::codec::test_host::HOST, &written_json)
        .expect("second read");

    assert_eq!(ir1, ir2, "decoded IR must be identical after round-trip");
}

#[test]
fn test_gemini_decode() {
    let j = serde_json::json!({
        "systemInstruction": {
            "parts": [{"text": "You are a helpful assistant."}]
        },
        "contents": [
            {"role": "user", "parts": [
                {"text": "What is the weather?"},
                {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"}}
            ]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"location": "San Francisco"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"temperature": 72, "units": "F"}}}
            ]}
        ],
        "tools": [{
            "functionDeclarations": [
                {
                    "name": "get_weather",
                    "description": "Get weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            ]
        }],
        "generationConfig": {
            "maxOutputTokens": 4096,
            "temperature": 0.7
        },
        "stream": true
    });

    let reader = GeminiReader;
    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");

    assert_eq!(ir.system.len(), 1);
    if let crate::codec::ir::IrBlock::Text {
        text,
        cache_control: _,
        citations: _,
        refusal: _,
    } = &ir.system[0]
    {
        assert_eq!(text, "You are a helpful assistant.");
    } else {
        panic!("expected Text block in system");
    }

    assert_eq!(ir.messages.len(), 3);

    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    assert_eq!(ir.messages[0].content.len(), 2);
    if let crate::codec::ir::IrBlock::Text { text, .. } = &ir.messages[0].content[0] {
        assert_eq!(text, "What is the weather?");
    } else {
        panic!("expected Text block in first message");
    }
    if let crate::codec::ir::IrBlock::Image {
        source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
        ..
    } = &ir.messages[0].content[1]
    {
        assert_eq!(media_type, "image/png");
        assert_eq!(data, "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ");
    } else {
        panic!("expected Image block in first message");
    }

    assert_eq!(ir.messages[1].role, crate::codec::ir::IrRole::Assistant);
    assert_eq!(ir.messages[1].content.len(), 1);
    let call_id = if let crate::codec::ir::IrBlock::ToolUse {
        id, name, input, ..
    } = &ir.messages[1].content[0]
    {
        assert_eq!(name, "get_weather");
        assert_eq!(
            input.get("location").and_then(|v| v.as_str()),
            Some("San Francisco")
        );
        id.clone()
    } else {
        panic!("expected ToolUse block in second message");
    };

    assert_eq!(ir.messages[2].role, crate::codec::ir::IrRole::User);
    assert_eq!(ir.messages[2].content.len(), 1);
    if let crate::codec::ir::IrBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
        ..
    } = &ir.messages[2].content[0]
    {
        assert_eq!(tool_use_id, &call_id);
        assert!(!is_error);
        assert_eq!(content.len(), 1);
        if let crate::codec::ir::IrBlock::Text { text, .. } = &content[0] {
            assert!(text.contains("72") || text.contains("temperature"));
        } else {
            panic!("expected Text block in tool result");
        }
    } else {
        panic!("expected ToolResult block in third message");
    }

    assert_eq!(ir.tools.len(), 1);
    let crate::codec::ir::IrTool {
        name,
        description,
        input_schema,
        ..
    } = &ir.tools[0];
    {
        assert_eq!(name, "get_weather");
        assert_eq!(description.as_deref(), Some("Get weather for a location"));
        assert!(!input_schema.is_null());
    }

    assert_eq!(ir.max_tokens, Some(4096));
    assert_eq!(ir.temperature, Some(0.7));
    assert!(ir.stream);
}

#[test]
fn test_gemini_roundtrip_identity() {
    let j = serde_json::json!({
        "model": "gemini-pro",
        "systemInstruction": {"parts": [{"text": "You are a helpful assistant."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]},
            {"role": "model", "parts": [{"text": "Hi there!"}]}
        ],
        "generationConfig": {"maxOutputTokens": 100, "temperature": 0.5},
        "stream": false
    });

    let reader = GeminiReader;
    let writer = GeminiWriter;

    let ir = reader
        .read_request(&j)
        .expect("read_request should succeed");
    let roundtrip = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(roundtrip, j, "round-trip must be byte-identical");
}

#[test]
fn test_gemini_protocol_resolves() {
    let proto = Protocol::gemini();
    assert_eq!(proto.name_static(), "gemini");

    let reader = proto.reader();
    let writer = proto.writer();

    let j = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "test"}]}]
    });
    let ir = reader.read_request(&j).expect("reader should parse");
    assert_eq!(ir.messages.len(), 1);

    let output = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        output["contents"][0]["parts"][0]["text"], "test",
        "the user turn must survive to the gemini wire, got {output}"
    );

    assert_eq!(writer.upstream_path(), "/v1beta/models");
    assert_eq!(
        writer.upstream_path_for("gemini-pro"),
        "/v1beta/models/gemini-pro:generateContent"
    );

    let raw = busbar_contract::protocol::CredentialHeader::Raw {
        header: "x-goog-api-key",
        trim_start: false,
    };
    assert!(matches!(
        decl_for("gemini").expect("gemini declares itself").egress_scheme,
        Some(busbar_contract::protocol::EgressScheme::Static { families: [], own, passthrough })
            if own == raw && passthrough == raw
    ));

    let status_code = StatusCode::TOO_MANY_REQUESTS;
    let signal = reader.classify(status_code, b"{}");
    assert_eq!(signal.class, StatusClass::RateLimit);

    let raw_error = reader.extract_error(status_code, b"{}");
    assert_eq!(raw_error.http_status, 429);
}

#[test]
fn test_bedrock_and_responses_register() {
    assert!(
        crate::codec::proto_codec::protocol_for("bedrock").is_some(),
        "bedrock in registry"
    );
    assert!(
        crate::codec::proto_codec::protocol_for("responses").is_some(),
        "responses in registry"
    );
    assert!(
        protocol_for("bedrock").is_some(),
        "bedrock resolves for ingress"
    );
    assert!(
        protocol_for("responses").is_some(),
        "responses resolves for ingress"
    );

    let responses = Protocol::responses();
    assert_eq!(responses.name_static(), "responses");
    assert_eq!(responses.writer().upstream_path(), "/v1/responses");

    let bearer = busbar_contract::protocol::CredentialHeader::Bearer;
    assert!(matches!(
        decl_for("responses").expect("responses declares itself").egress_scheme,
        Some(busbar_contract::protocol::EgressScheme::Static { families: [], own, passthrough })
            if own == bearer && passthrough == bearer
    ));

    let gemini = Protocol::gemini();
    assert_eq!(
        gemini
            .writer()
            .upstream_path_for_stream("gemini-pro", false),
        "/v1beta/models/gemini-pro:generateContent"
    );
    assert_eq!(
        gemini.writer().upstream_path_for_stream("gemini-pro", true),
        "/v1beta/models/gemini-pro:streamGenerateContent?alt=sse"
    );

    assert_eq!(
        Protocol::openai()
            .writer()
            .upstream_path_for_stream("x", true),
        "/v1/chat/completions"
    );
    assert_eq!(
        Protocol::openai().writer().upstream_path_for("x"),
        "/v1/chat/completions"
    );

    let bedrock = Protocol::bedrock();
    assert_eq!(bedrock.name_static(), "bedrock");
    assert_eq!(
        bedrock.writer().upstream_path_for("anthropic.claude-3"),
        "/model/anthropic.claude-3/converse"
    );
}

/// SAME-PROTOCOL verbatim strip, OPTED-OUT client: busbar forces
#[test]
fn same_proto_openai_opted_out_strips_trailing_usage_chunk() {
    let mut st = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    st.set_client_include_usage(false);

    let frames = [
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"total_tokens\":14}}\n\n",
        "data: [DONE]\n\n",
    ];
    let mut raw: Vec<u8> = Vec::new();
    for f in frames {
        raw.extend(st.feed(crate::codec::test_host::HOST, f.as_bytes()));
    }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    assert!(
        out.contains("\"content\":\"Hi\""),
        "content chunk kept:\n{out}"
    );
    assert!(
        out.contains("\"finish_reason\":\"stop\""),
        "finish chunk kept:\n{out}"
    );
    assert!(out.contains("[DONE]"), "terminator kept:\n{out}");

    for data in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if data.trim() == "[DONE]" {
            continue;
        }
        let v: serde_json::Value =
            crate::codec::json::parse(data.as_bytes()).expect("each SSE chunk is valid JSON");
        let usage_obj = v.get("usage").is_some_and(|u| u.is_object());
        let choices_empty = v
            .get("choices")
            .and_then(|c| c.as_array())
            .is_some_and(|a| a.is_empty());
        assert!(
            !(usage_obj && choices_empty),
            "opted-out client must NOT receive the trailing usage-only chunk; got:\n{data}"
        );
    }

    let u = st
        .usage()
        .expect("A-tap usage must be populated for billing");
    assert_eq!(u.output_tokens, 3, "billing output tokens from A-tap");

    assert_eq!(u.input_tokens, 11, "billing input tokens from A-tap");
}

/// SAME-PROTOCOL verbatim, OPTED-IN client: when the client DID send
#[test]
fn same_proto_openai_opted_in_keeps_trailing_usage_chunk() {
    let mut st = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    st.set_client_include_usage(true);
    let frames = [
        "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Yo\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
        "data: [DONE]\n\n",
    ];
    let mut raw: Vec<u8> = Vec::new();
    for f in frames {
        raw.extend(st.feed(crate::codec::test_host::HOST, f.as_bytes()));
    }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    let has_trailing = out
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .any(|data| {
            if data.trim() == "[DONE]" {
                return false;
            }
            let v: serde_json::Value =
                crate::codec::json::parse(data.as_bytes()).expect("valid JSON");
            v.get("usage").is_some_and(|u| u.is_object())
                && v.get("choices")
                    .and_then(|c| c.as_array())
                    .is_some_and(|a| a.is_empty())
        });
    assert!(
        has_trailing,
        "opted-in client MUST receive the trailing usage-only chunk verbatim:\n{out}"
    );

    let u = st.usage().expect("A-tap usage");
    assert_eq!(u.output_tokens, 2);
    assert_eq!(u.input_tokens, 7);
}

/// INDISTINGUISHABILITY (same-proto OpenAI->OpenAI, OPTED-OUT client): busbar forces
#[test]
fn same_proto_openai_opted_out_strips_usage_from_every_chunk() {
    let mut st = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    st.set_client_include_usage(false);

    let frames = [
        "data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"total_tokens\":14}}\n\n",
        "data: [DONE]\n\n",
    ];
    let mut raw: Vec<u8> = Vec::new();
    for f in frames {
        raw.extend(st.feed(crate::codec::test_host::HOST, f.as_bytes()));
    }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    for data in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if data.trim() == "[DONE]" {
            continue;
        }
        let v: serde_json::Value =
            crate::codec::json::parse(data.as_bytes()).expect("each SSE chunk is valid JSON");
        assert!(
            v.get("usage").is_none(),
            "opted-out client must see NO `usage` key on any chunk; got:\n{data}"
        );
    }

    assert!(
        !out.contains("\"usage\""),
        "no `usage` key byte-substring may remain in the client stream:\n{out}"
    );

    assert!(
        out.contains("\"content\":\"Hel\""),
        "content 1 kept:\n{out}"
    );
    assert!(out.contains("\"content\":\"lo\""), "content 2 kept:\n{out}");
    assert!(
        out.contains("\"finish_reason\":\"stop\""),
        "finish kept:\n{out}"
    );
    assert!(out.contains("\"model\":\"gpt-4o\""), "model kept:\n{out}");
    assert!(out.contains("[DONE]"), "terminator kept:\n{out}");

    let u = st
        .usage()
        .expect("A-tap usage must be populated for billing");
    assert_eq!(u.output_tokens, 3, "billing output tokens from A-tap");
    assert_eq!(u.input_tokens, 11, "billing input tokens from A-tap");
}

/// INDISTINGUISHABILITY (same-proto OpenAI->OpenAI, OPTED-IN client): a client that DID request
#[test]
fn same_proto_openai_opted_in_keeps_usage_on_every_chunk() {
    let mut st = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    st.set_client_include_usage(true);
    let frames = [
        "data: {\"id\":\"chatcmpl-8\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Yo\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-8\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-8\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
        "data: [DONE]\n\n",
    ];
    let mut raw: Vec<u8> = Vec::new();
    for f in frames {
        raw.extend(st.feed(crate::codec::test_host::HOST, f.as_bytes()));
    }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    for f in &frames[..3] {
        assert!(
            out.contains(f.trim_end()),
            "opted-in client must receive frame verbatim: {f}\nout:\n{out}"
        );
    }

    assert!(
        out.contains("\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}"),
        "opted-in client MUST receive the trailing usage object verbatim:\n{out}"
    );

    let u = st.usage().expect("A-tap usage");
    assert_eq!(u.output_tokens, 2);
    assert_eq!(u.input_tokens, 7);
}

/// INDISTINGUISHABILITY (byte-safety, string-content corruption guard): a content chunk whose message
#[test]
fn same_proto_openai_opted_out_preserves_usage_text_in_content() {
    let mut st = StreamTranslate::new_same_proto("openai").expect("same-proto translator");
    st.set_client_include_usage(false);
    let frames = [
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the tell is \\\"usage\\\":null on every chunk\"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: [DONE]\n\n",
    ];
    let mut raw: Vec<u8> = Vec::new();
    for f in frames {
        raw.extend(st.feed(crate::codec::test_host::HOST, f.as_bytes()));
    }
    raw.extend(st.finish(crate::codec::test_host::HOST));
    let out = String::from_utf8(raw).expect("utf8 SSE");

    for data in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if data.trim() == "[DONE]" {
            continue;
        }
        let v: serde_json::Value = crate::codec::json::parse(data.as_bytes()).expect("valid JSON");

        assert!(
            v.get("usage").is_none(),
            "top-level usage stripped:\n{data}"
        );
    }

    assert!(
        out.contains("the tell is \\\"usage\\\":null on every chunk"),
        "message content containing the literal `usage:null` text must survive:\n{out}"
    );
}

/// A translated cross-protocol Anthropic-INGRESS stream must emit `event: ping`
#[test]
fn anthropic_cross_protocol_stream_emits_ping_after_message_start() {
    let mut t = StreamTranslate::new("anthropic", "openai").expect("translator");
    let out = String::from_utf8(t.feed(
        crate::codec::test_host::HOST,
        b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
    ))
    .unwrap();
    let start_pos = out
        .find("event: message_start")
        .expect("a message_start frame must be emitted; got {out}");
    let ping_pos = out
        .find("event: ping")
        .expect("a translated Anthropic-ingress stream must emit event: ping; got: {out}");
    assert!(
        ping_pos > start_pos,
        "the ping must come AFTER message_start; got: {out}"
    );
    assert!(
        out[ping_pos..].starts_with("event: ping\ndata: {\"type\":\"ping\"}\n\n"),
        "the ping frame must be Anthropic's native shape; got: {}",
        &out[ping_pos..]
    );
}

/// On the Anthropic same-proto verbatim path, only the two usage-bearing event types
#[test]
fn same_proto_anthropic_skips_decode_for_non_usage_frames() {
    let mut t = StreamTranslate::new_same_proto("anthropic").expect("translator");
    let frames = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"b\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"c\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"d\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"e\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
    );
    let out = t.feed(crate::codec::test_host::HOST, frames.as_bytes());

    assert_eq!(
        out,
        frames.as_bytes(),
        "same-proto Anthropic bytes must round-trip verbatim"
    );
    assert_eq!(
        t.decode_calls.get(),
        2,
        "only message_start and message_delta should reach the DOM parse; \
         content_block_start/delta/stop must be skipped entirely"
    );

    let usage = t.usage().expect("terminal usage must still be captured");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 5);
}

/// Every same-proto SSE dialect reads only the frames that can carry its usage or terminal
#[test]
fn same_proto_reads_only_usage_and_failure_frames() {
    let cases: [(&str, &str, usize); 4] = [
        (
            "openai",
            concat!(
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}],\"usage\":null}\n\n",
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"usage\"},\"finish_reason\":null}],\"usage\":null}\n\n",
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
                "data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n",
                "data: [DONE]\n\n",
            ),
            2,
        ),
        (
            "gemini",
            concat!(
                "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"a\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":1}}\n\n",
                "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"b\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3,\"totalTokenCount\":10}}\n\n",
            ),
            1,
        ),
        (
            "cohere",
            concat!(
                "event: message-start\ndata: {\"type\":\"message-start\",\"id\":\"m\"}\n\n",
                "event: content-delta\ndata: {\"type\":\"content-delta\",\"index\":0,\"delta\":{\"message\":{\"content\":{\"text\":\"message-end\"}}}}\n\n",
                "event: message-end\ndata: {\"type\":\"message-end\",\"delta\":{\"finish_reason\":\"COMPLETE\",\"usage\":{\"tokens\":{\"input_tokens\":7,\"output_tokens\":3}}}}\n\n",
            ),
            1,
        ),
        (
            "responses",
            concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"a\"}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n",
            ),
            1,
        ),
    ];
    for (dialect, frames, reads) in cases {
        let mut t = StreamTranslate::new_same_proto(dialect).expect("translator");
        t.set_client_include_usage(true);
        let out = t.feed_borrowed(crate::codec::test_host::HOST, frames.as_bytes());
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(b) if b == frames.as_bytes()),
            "{dialect}: whole frames needing no edit relay borrowed and verbatim"
        );
        assert_eq!(t.decode_calls.get(), reads, "{dialect}: frames parsed");
        let usage = t.usage().expect("the stream's usage is still read");
        assert_eq!(usage.input_tokens, 7, "{dialect}");
        assert_eq!(usage.output_tokens, 3, "{dialect}");
        assert!(t.terminal_error().is_none(), "{dialect}");
    }
}

/// THE TERMINAL-USAGE FOLD CARRIES THE SUB-BUCKETS, end to end through the translator. The
#[test]
fn fold_path_merges_trailing_usage_detail_sub_buckets_end_to_end() {
    let mut st = StreamTranslate::new("responses", "openai")
        .expect("responses<-openai is a real cross-protocol pair");

    let wire = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"o3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"o3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"o3\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30,\"completion_tokens_details\":{\"reasoning_tokens\":9}}}\n\n",
        "data: [DONE]\n\n",
    );
    let mut out = st.feed(crate::codec::test_host::HOST, wire.as_bytes());
    out.extend_from_slice(&st.finish(crate::codec::test_host::HOST));
    let frames = decode_wire_frames(false, &out);
    let completed = frames
        .iter()
        .find(|(ev, _)| ev == "response.completed")
        .map(|(_, json)| json)
        .expect("the responses stream ends with response.completed");
    let usage = completed
        .pointer("/response/usage")
        .expect("response.completed carries usage");
    assert_eq!(usage["input_tokens"], 10, "{usage}");
    assert_eq!(usage["output_tokens"], 20, "{usage}");
    assert_eq!(
        usage["output_tokens_details"]["reasoning_tokens"], 9,
        "the trailing chunk's reasoning sub-bucket survives the terminal-usage fold — the \
         streamed answer must match the buffered one: {usage}"
    );
}

/// Buffered-2xx-wrapped-as-a-stream must remap each tool id EXACTLY ONCE.
#[test]
fn test_buffered_as_stream_remaps_tool_ids_exactly_once() {
    let mut ir = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            id: "call_abc".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "Paris"}),
            cache_control: None,
            thought_signature: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 5,
            output_tokens: 2,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("test-model".to_string()),
        id: Some("chatcmpl-x".to_string()),
        created: Some(1_700_000_000),
        system_fingerprint: None,
        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    crate::codec::chat_handle::chat_prepare_for_ingress(&mut ir, "anthropic", 1_700_000_001);
    let prepared_id = match &ir.content[0] {
        crate::codec::ir::IrBlock::ToolUse { id, .. } => id.clone(),
        other => panic!("expected a tool_use block, got {other:?}"),
    };
    assert_eq!(
        decode_native_tool_id("anthropic", &prepared_id).as_deref(),
        Some("call_abc"),
        "precondition: the prepared answer already carries a once-encoded native id"
    );

    let bytes = StreamTranslate::synthesize_from_response(
        crate::codec::test_host::HOST,
        "anthropic",
        &ir,
        busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    )
    .expect("anthropic is SSE-framed, so the shared synthesizer answers");
    let out = String::from_utf8(bytes).expect("utf-8 SSE");
    let emitted = data_payloads(&out)
        .into_iter()
        .find_map(|p| {
            p.pointer("/content_block/id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .expect("a content_block_start carrying a tool_use id");

    assert_eq!(
        emitted, prepared_id,
        "the synthesized stream must carry the SAME id the buffered write carries — the \
         synthesizer drives an already-prepared answer and must not re-encode it"
    );
    assert_eq!(
        decode_native_tool_id("anthropic", &emitted).as_deref(),
        Some("call_abc"),
        "one decode must recover the original egress id; a doubly-wrapped id leaves a layer \
         behind and the backend rejects the next turn's tool_result: {emitted}"
    );
}

/// A Responses stream that ends on `response.failed` still REPORTS the usage the far end generated
#[test]
fn responses_failed_stream_bills_its_reported_usage() {
    let created = b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"m\",\"output\":[],\"usage\":null}}\n\n";
    let failed = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"failed\",\"model\":\"m\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"boom\"},\"usage\":{\"input_tokens\":300,\"output_tokens\":2000,\"total_tokens\":2300}}}\n\n";

    let mut same = StreamTranslate::new_same_proto("responses").expect("same-proto translator");
    let _ = same.feed(crate::codec::test_host::HOST, created);
    let _ = same.feed(crate::codec::test_host::HOST, failed);
    let _ = same.finish(crate::codec::test_host::HOST);
    let u = same
        .usage()
        .expect("same-protocol: the usage a response.failed reports must bill");
    assert_eq!(u.input_tokens, 300);
    assert_eq!(u.output_tokens, 2000);
    assert!(same.terminal_error().is_some(), "the stream still failed");

    let mut cross = StreamTranslate::new("anthropic", "responses").expect("cross-proto translator");
    let _ = cross.feed(crate::codec::test_host::HOST, created);
    let _ = cross.feed(crate::codec::test_host::HOST, failed);
    let _ = cross.finish(crate::codec::test_host::HOST);
    let u = cross
        .usage()
        .expect("cross-protocol: the usage a response.failed reports must bill");
    assert_eq!(u.input_tokens, 300);
    assert_eq!(u.output_tokens, 2000);
    assert!(cross.terminal_error().is_some(), "the stream still failed");

    let mut none = StreamTranslate::new_same_proto("responses").expect("same-proto translator");
    let _ = none.feed(crate::codec::test_host::HOST, created);
    let _ = none.feed(crate::codec::test_host::HOST, b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"failed\",\"model\":\"m\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"boom\"},\"usage\":null}}\n\n");
    let _ = none.finish(crate::codec::test_host::HOST);
    assert!(none.usage().is_none());
    assert!(none.terminal_error().is_some());
}

/// A TRANSLATED STREAM INTO A RESPONSES CLIENT IS STAMPED WITH THE HOST'S CLOCK, NOT 1970. The
#[test]
fn a_translated_stream_into_responses_carries_the_host_clock_created_at() {
    let host = crate::codec::test_host::HOST;
    let before = host.wall_clock_now().expect("the test host holds a clock");
    let mut st = StreamTranslate::new("responses", "anthropic").expect("translator");
    let mut out = st.feed(
        host,
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    );
    out.extend(st.feed(
        host,
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ));
    out.extend(st.finish(host));
    let text = String::from_utf8_lossy(&out);
    let stamps: Vec<u64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| v["response"]["created_at"].as_u64())
        .collect();
    assert!(
        stamps.len() >= 2,
        "response.created and the terminal event carry created_at: {text}"
    );
    for s in &stamps {
        assert!(
            *s >= before,
            "created_at {s} is the host clock, never 0: {text}"
        );
    }
    assert!(
        stamps.windows(2).all(|w| w[0] == w[1]),
        "one created_at across the stream: {stamps:?}"
    );
}

#[test]
fn an_anthropic_tool_call_id_reaches_an_openai_client_remapped_and_unleaked() {
    let ir_resp = AnthropicReader
        .read_response(
            crate::codec::test_host::HOST,
            &serde_json::json!({
                "id": "msg_x",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_origRAW123",
                    "name": "get_weather",
                    "input": {"city": "SF"}
                }],
                "model": "claude-3",
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }),
        )
        .expect("anthropic read");
    let mut ir_resp = ir_resp;
    crate::codec::chat_handle::chat_prepare_for_ingress(&mut ir_resp, "openai", 1_752_000_000);
    let body = crate::codec::proto_codec::protocol_for("openai")
        .expect("openai")
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir_resp);
    let tool_id = body
        .pointer("/choices/0/message/tool_calls/0/id")
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| panic!("expected an OpenAI tool_calls id in: {body}"))
        .to_string();
    assert!(tool_id.starts_with("call_"), "{tool_id}");
    assert_ne!(tool_id, "toolu_origRAW123");
    let bytes = serde_json::to_vec(&body).expect("serializes");
    assert!(
        !bytes
            .windows("toolu_origRAW123".len())
            .any(|w| w == b"toolu_origRAW123"),
        "the raw Anthropic tool id must not appear anywhere in the client body: {body}"
    );
    assert_eq!(
        decode_native_tool_id("openai", &tool_id).as_deref(),
        Some("toolu_origRAW123")
    );
}

/// The native tap preserves provider bytes while both paths retain the terminal failure.
#[test]
fn a_top_level_responses_error_is_terminal_on_native_and_translated_streams() {
    let host = crate::codec::test_host::HOST;
    let created = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_error\",\"model\":\"m\",\"status\":\"in_progress\"}}\n\n";
    let failed = b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"boom\"}\n\n";
    let mut same = StreamTranslate::new_same_proto("responses").expect("native tap");
    let mut output = same.feed(host, created);
    output.extend(same.feed(host, failed));
    output.extend(same.finish(host));
    let mut expected = created.to_vec();
    expected.extend_from_slice(failed);
    assert_eq!(output, expected, "native error bytes must pass unchanged");
    assert!(
        same.terminal_error().is_some(),
        "the native tap must observe the error"
    );
    assert!(
        same.usage().is_none(),
        "an error without usage cannot invent counts"
    );
    let mut cross = StreamTranslate::new("anthropic", "responses").expect("translated stream");
    let mut output = cross.feed(host, created);
    output.extend(cross.feed(host, failed));
    output.extend(cross.finish(host));
    assert!(
        cross.terminal_error().is_some(),
        "translation must retain the error"
    );
    assert!(
        cross.usage().is_none(),
        "an error without usage cannot invent counts"
    );
    assert!(
        String::from_utf8_lossy(&output).contains("event: error"),
        "{}",
        String::from_utf8_lossy(&output)
    );
}
