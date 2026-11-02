//! A requested stream served from a whole upstream body, through the actual reply path.

use busbar_contract::codec::CodecHost;
use busbar_contract::operation::OpVerb;
use busbar_contract::protocol::HeadFields;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Piece, Reply, ReplyCtx};
use busbar_plane_llm::exchange::shaping::Lane;
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
            wants_stream: true,
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
            let (_, data) = busbar_plane_llm::codec::dialect::parse_sse_frame(frame.as_bytes())?;
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
    for ingress in ["openai", "responses"] {
        let request = json!({"model": "model", "stream": true});
        let piece = reply(
            ingress,
            request,
            &[(b"content-type", b"application/json")],
            &far,
        );
        assert!(piece.synthesized, "{ingress}");
        let arguments: String = frames(&piece.bytes)
            .iter()
            .filter_map(|frame| match ingress {
                "openai" => frame
                    .pointer("/choices/0/delta/tool_calls/0/function/arguments")?
                    .as_str(),
                "responses" if frame["type"] == "response.function_call_arguments.delta" => {
                    frame["delta"].as_str()
                }
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
