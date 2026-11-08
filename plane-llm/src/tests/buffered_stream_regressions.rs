//! A requested stream served from a whole upstream body, through the actual reply path.

use crate::codec::cells::CodecHost;
use crate::codec::protocol::HeadFields;
use crate::exchange::arrive::Arrived;
use crate::exchange::attempt::StreamIntent;
use crate::exchange::reply::{At, Piece, Reply, ReplyCtx};
use crate::exchange::shaping::Lane;
use busbar_contract::operation::OpVerb;
use serde_json::{json, Value};

struct Host;

impl CodecHost for Host {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }

    fn wall_clock_now(&self) -> Option<u64> {
        Some(1_700_000_000)
    }
}

fn reply<'a>(
    ingress: &'static str,
    request: Value,
    head: HeadFields<'_>,
    body: &'a [u8],
) -> Piece<'a> {
    reply_from(ingress, "openai", request, head, body)
}

fn reply_from<'a>(
    ingress: &'static str,
    egress: &'static str,
    request: Value,
    head: HeadFields<'_>,
    body: &'a [u8],
) -> Piece<'a> {
    let include_usage =
        request.pointer("/stream_options/include_usage") == Some(&Value::Bool(true));
    let wants_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let arrived = Arrived {
        dialect: ingress,
        operation: OpVerb::CHAT,
        model: "model".into(),
        content_type: "application/json".into(),
        body: serde_json::to_vec(&request).unwrap(),
        parsed: Some(request),
        path: "/v1/chat/completions".into(),
        query: None,
        path_model: None,
    };
    let lane = Lane {
        model: "model".into(),
        provider: "provider".into(),
        dialect: egress,
        path: None,
        path_base: None,
        options: Vec::new(),
        upstream_model: None,
        default_max_tokens: None,
        context_max: None,
        reasoning: false,
        prompt_caching: false,
        caps: Default::default(),
        error_map: Default::default(),
        statics: &[],
        invoke: false,
    };
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: StreamIntent {
            wants_stream,
            client_include_usage: include_usage,
        },
        passthrough: false,
        translate_cap: 1024 * 1024,
    };
    Reply::new(&Host, &ctx, 200, head).feed(&Host, &ctx, body, true, At::default())
}

fn frames(body: &[u8]) -> Vec<Value> {
    std::str::from_utf8(body)
        .unwrap()
        .split("\n\n")
        .filter_map(|frame| {
            let (_, data) = crate::codec::dialect::parse_sse_frame(frame.as_bytes())?;
            serde_json::from_str(&data).ok()
        })
        .collect()
}

#[test]
fn synthesized_tool_arguments_preserve_partial_json_text() {
    let raw = "{\"city\":";
    let far = serde_json::to_vec(&json!({
        "id": "chatcmpl-native", "object": "chat.completion", "model": "model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "call_native", "type": "function",
                "function": {"name": "weather", "arguments": raw}}]},
            "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12}
    }))
    .unwrap();
    for ingress in ["openai", "responses", "bedrock"] {
        let request = json!({"model": "model", "stream": true});
        let piece = reply(
            ingress,
            request,
            &[(b"content-type", b"application/json")],
            &far,
        );
        assert!(piece.synthesized, "{ingress}");
        let decoded = if ingress == "bedrock" {
            let mut body = piece.bytes.to_vec();
            crate::codec::eventstream::drain_frames_checked(&mut body, None)
                .0
                .into_iter()
                .map(|(_, payload)| serde_json::from_slice::<Value>(&payload).unwrap())
                .collect()
        } else {
            frames(&piece.bytes)
        };
        let arguments: String = decoded
            .iter()
            .filter_map(|frame| match ingress {
                "openai" => frame
                    .pointer("/choices/0/delta/tool_calls/0/function/arguments")?
                    .as_str(),
                "responses" if frame["type"] == "response.function_call_arguments.delta" => {
                    frame["delta"].as_str()
                }
                "bedrock" => frame.pointer("/delta/toolUse/input")?.as_str(),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, raw, "{ingress}");
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
    }
}

fn text_completion() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "chatcmpl-native", "object": "chat.completion", "model": "model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12}
    }))
    .unwrap()
}

#[test]
fn synthesized_chat_usage_obeys_the_original_client_opt_in() {
    let far = text_completion();
    for include in [false, true] {
        let request = json!({"model": "model", "stream": true,
            "stream_options": {"include_usage": include}});
        let piece = reply(
            "openai",
            request,
            &[(b"content-type", b"application/json")],
            &far,
        );
        assert!(piece.synthesized);
        let frames = frames(&piece.bytes);
        let usage: Vec<_> = frames
            .iter()
            .filter_map(|frame| frame.get("usage"))
            .collect();
        assert_eq!(usage.len(), usize::from(include));
        if include {
            assert_eq!(usage[0]["prompt_tokens"], 9);
            assert_eq!(usage[0]["completion_tokens"], 3);
        }
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
    }
}

#[test]
fn synthesized_responses_stream_echoes_the_original_request_members() {
    let far = text_completion();
    let request = json!({"model": "model", "stream": true, "input": "hi",
        "instructions": "Be concise", "temperature": 0.2, "top_p": 0.8,
        "metadata": {"caller": "fixture"}, "parallel_tool_calls": false,
        "tools": [{"type": "function", "name": "weather", "parameters": {"type": "object"}}],
        "tool_choice": "auto"});
    let piece = reply(
        "responses",
        request.clone(),
        &[(b"content-type", b"application/json")],
        &far,
    );
    assert!(piece.synthesized);
    let frames = frames(&piece.bytes);
    for kind in ["response.created", "response.completed"] {
        let frame = frames
            .iter()
            .find(|frame| frame["type"] == kind)
            .expect(kind);
        for key in [
            "instructions",
            "temperature",
            "top_p",
            "metadata",
            "parallel_tool_calls",
            "tools",
            "tool_choice",
        ] {
            assert_eq!(frame["response"][key], request[key], "{kind}.{key}");
        }
    }
}

#[test]
fn same_dialect_synthesis_keeps_native_headers_with_the_new_body_length() {
    let far = text_completion();
    let old_length = far.len().to_string();
    let head: HeadFields<'_> = &[
        (b"content-type", b"application/json"),
        (b"content-length", old_length.as_bytes()),
        (b"x-request-id", b"req-native"),
        (b"x-vendor-detail", b"keep"),
        (b"openai-organization", b"operator-tenant"),
        (b"set-cookie", b"operator-cookie"),
    ];
    let request = json!({"model": "model", "stream": true});
    let piece = reply("openai", request, head, &far);
    assert!(piece.synthesized);
    let fields = &piece.head.as_ref().unwrap().fields;
    let field = |name: &str| {
        fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    };
    assert_eq!(field("x-request-id"), Some(b"req-native".as_slice()));
    assert_eq!(field("x-vendor-detail"), Some(b"keep".as_slice()));
    assert_eq!(field("openai-organization"), None);
    assert_eq!(field("set-cookie"), None);
    assert_eq!(field("content-type"), Some(b"text/event-stream".as_slice()));
    let length = piece.bytes.len().to_string();
    assert_eq!(field("content-length"), Some(length.as_bytes()));
    assert_ne!(piece.bytes.len(), far.len(), "the synthesized body changed");
}

#[test]
fn same_dialect_synthesis_replaces_the_renderers_minted_request_id() {
    let far = serde_json::to_vec(&json!({
        "id": "msg-native", "type": "message", "role": "assistant", "model": "model",
        "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 9, "output_tokens": 3}
    }))
    .unwrap();
    let request = json!({"model": "model", "stream": true});
    let piece = reply_from(
        "anthropic",
        "anthropic",
        request,
        &[
            (b"content-type", b"application/json"),
            (b"request-id", b"req_native"),
        ],
        &far,
    );
    assert!(piece.synthesized);
    let ids: Vec<_> = piece
        .head
        .as_ref()
        .unwrap()
        .fields
        .iter()
        .filter(|(name, _)| name == "request-id")
        .map(|(_, value)| value.as_slice())
        .collect();
    assert_eq!(ids, [b"req_native".as_slice()]);
}

fn aws_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 12];
    for (name, value) in headers {
        frame.push(u8::try_from(name.len()).unwrap());
        frame.extend_from_slice(name.as_bytes());
        frame.push(7);
        frame.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(value.as_bytes());
    }
    let header_len = u32::try_from(frame.len() - 12).unwrap();
    let total = u32::try_from(frame.len() + payload.len() + 4).unwrap();
    frame[..4].copy_from_slice(&total.to_be_bytes());
    frame[4..8].copy_from_slice(&header_len.to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame[..8]);
    frame[8..12].copy_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(payload);
    let crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

#[test]
fn explicit_aws_error_headers_fail_the_reply_independently_of_payload_and_code() {
    use crate::codec::eventstream::encode_frame;
    use crate::exchange::reply::{Fault, Verdict};

    let mut prefix = encode_frame("messageStart", br#"{"role":"assistant"}"#);
    prefix.extend(encode_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"text":"partial"}}"#,
    ));
    prefix.extend(encode_frame(
        "metadata",
        br#"{"usage":{"inputTokens":9,"outputTokens":3,"totalTokens":12}}"#,
    ));
    for code in [
        None,
        Some("InternalFailure"),
        Some("FutureUnmodeledFailure"),
    ] {
        let mut headers = vec![
            (":message-type", "error"),
            (":error-message", "upstream terminated"),
        ];
        if let Some(code) = code {
            headers.push((":error-code", code));
        }
        for payload in [b"".as_slice(), b"{}".as_slice(), b"not JSON".as_slice()] {
            let mut far = prefix.clone();
            far.extend(aws_frame(&headers, payload));
            for ingress in [
                "bedrock",
                "openai",
                "anthropic",
                "responses",
                "gemini",
                "cohere",
            ] {
                let piece = reply_from(
                    ingress,
                    "bedrock",
                    json!({"stream":true}),
                    &[(b"content-type", b"application/vnd.amazon.eventstream")],
                    &far,
                );
                assert_eq!(piece.verdict, Verdict::Hard, "{ingress}: {code:?}");
                assert_eq!(
                    piece.fault,
                    Some(Fault::Transient("stream-terminal-error")),
                    "{ingress}"
                );
                assert_eq!(
                    (piece.units.tokens_in, piece.units.tokens_out),
                    (0, 0),
                    "{ingress}"
                );
                if ingress == "bedrock" {
                    assert_eq!(piece.bytes.as_ref(), far, "native frames remain verbatim");
                }
            }
        }
    }
}

#[test]
fn an_unknown_normal_aws_event_is_still_skipped_without_failing_the_reply() {
    use crate::exchange::reply::Verdict;
    let far = aws_frame(
        &[
            (":message-type", "event"),
            (":event-type", "InternalFailure"),
        ],
        b"not JSON",
    );
    let piece = reply_from(
        "openai",
        "bedrock",
        json!({"stream":true}),
        &[(b"content-type", b"application/vnd.amazon.eventstream")],
        &far,
    );
    assert_eq!(piece.verdict, Verdict::Ok);
    assert_eq!(piece.fault, None);
}

#[test]
fn unknown_aws_events_do_not_interrupt_a_complete_reply() {
    use crate::codec::eventstream::encode_frame;
    use crate::exchange::reply::Verdict;

    let unknown = aws_frame(
        &[
            (":message-type", "event"),
            (":event-type", "InternalFailure"),
        ],
        b"not JSON",
    );
    let mut far = unknown.clone();
    far.extend(encode_frame("messageStart", br#"{"role":"assistant"}"#));
    far.extend(encode_frame(
        "contentBlockDelta",
        br#"{"contentBlockIndex":0,"delta":{"text":"hello"}}"#,
    ));
    far.extend(unknown.clone());
    far.extend(encode_frame(
        "contentBlockStop",
        br#"{"contentBlockIndex":0}"#,
    ));
    far.extend(encode_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
    far.extend(encode_frame(
        "metadata",
        br#"{"usage":{"inputTokens":9,"outputTokens":3,"totalTokens":12}}"#,
    ));
    far.extend(unknown);
    let piece = reply_from(
        "openai",
        "bedrock",
        json!({"stream":true}),
        &[(b"content-type", b"application/vnd.amazon.eventstream")],
        &far,
    );
    assert_eq!(piece.verdict, Verdict::Ok);
    assert_eq!(piece.fault, None);
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
    let decoded = frames(&piece.bytes);
    assert!(decoded
        .iter()
        .any(|v| v.pointer("/choices/0/delta/content") == Some(&json!("hello"))));
    assert!(decoded
        .iter()
        .any(|v| v.pointer("/choices/0/finish_reason") == Some(&json!("stop"))));
}

#[test]
fn malformed_known_aws_events_still_fail_translation() {
    use crate::codec::eventstream::encode_frame;
    use crate::exchange::reply::{Fault, Verdict};

    for event in [
        "messageStart",
        "contentBlockStart",
        "contentBlockDelta",
        "contentBlockStop",
        "messageStop",
        "metadata",
    ] {
        let far = encode_frame(event, b"not JSON");
        let piece = reply_from(
            "openai",
            "bedrock",
            json!({"stream":true}),
            &[(b"content-type", b"application/vnd.amazon.eventstream")],
            &far,
        );
        assert_eq!(piece.verdict, Verdict::Hard, "{event}");
        assert_eq!(
            piece.fault,
            Some(Fault::Transient("stream-translate-abort")),
            "{event}"
        );
    }
}

#[test]
fn invoke_header_only_errors_keep_the_native_error_message() {
    let far = aws_frame(
        &[
            (":message-type", "error"),
            (":error-code", "ThrottlingException"),
            (":error-message", "slow down"),
        ],
        b"",
    );
    let mut transcoder = crate::exchange::invoke::Transcoder::new();
    let decoded = frames(&transcoder.feed(&far));
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0]["error"]["type"], "rate_limit_error");
    assert_eq!(decoded[0]["error"]["message"], "slow down");
}

#[test]
fn generated_media_loss_is_named_on_buffered_and_synthesized_target_replies() {
    let far = serde_json::to_vec(&json!({"candidates":[{"content":{"role":"model","parts":[
        {"text":"hello"}, {"inlineData":{"mimeType":"audio/wav","data":"YQ=="}}
    ]},"finishReason":"STOP"}], "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}}))
    .unwrap();
    for ingress in ["openai", "bedrock"] {
        for stream in [false, true] {
            let piece = reply_from(
                ingress,
                "gemini",
                json!({"stream":stream}),
                &[(b"content-type", b"application/json")],
                &far,
            );
            assert_eq!(
                piece.head.as_ref().unwrap().status,
                200,
                "{ingress}/{stream}"
            );
            assert!(
                piece.bytes.windows(5).any(|bytes| bytes == b"hello"),
                "{ingress}/{stream}"
            );
            assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
            assert!(
                piece
                    .dropped
                    .iter()
                    .any(|path| path == "candidates[].content.parts[]"),
                "{ingress}/{stream}: {:?}",
                piece.dropped
            );
        }
    }
}

#[test]
fn generated_media_loss_is_named_on_live_target_replies() {
    let event = json!({"candidates":[{"content":{"role":"model","parts":[
        {"text":"hello"}, {"inlineData":{"mimeType":"audio/wav","data":"YQ=="}}
    ]},"finishReason":"STOP"}], "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}});
    let far = format!("data: {event}\n\n");
    for ingress in ["openai", "bedrock"] {
        let piece = reply_from(
            ingress,
            "gemini",
            json!({"stream":true}),
            &[(b"content-type", b"text/event-stream")],
            far.as_bytes(),
        );
        assert_eq!(piece.head.as_ref().unwrap().status, 200, "{ingress}");
        assert!(
            piece.bytes.windows(5).any(|bytes| bytes == b"hello"),
            "{ingress}"
        );
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
        assert!(
            piece
                .dropped
                .iter()
                .any(|path| path == "candidates[].content.parts[]"),
            "{ingress}: {:?}",
            piece.dropped
        );
    }
}

#[test]
fn top_level_native_audio_loss_is_named_on_foreign_target_replies() {
    let far = br#"{"id":"native","object":"chat.completion","model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"hello","audio":{"id":"audio","data":"YQ==","expires_at":1,"transcript":"hello"}},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":3,"total_tokens":12}}"#;
    for ingress in ["responses", "bedrock"] {
        for stream in [false, true] {
            let piece = reply_from(
                ingress,
                "openai",
                json!({"stream":stream}),
                &[(b"content-type", b"application/json")],
                far,
            );
            assert_eq!(piece.head.as_ref().unwrap().status, 200);
            assert!(piece.bytes.windows(5).any(|bytes| bytes == b"hello"));
            assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
            assert!(
                piece.dropped.iter().any(|path| path.contains("audio")),
                "{ingress}/{stream}: {:?}",
                piece.dropped
            );
        }
    }
}
