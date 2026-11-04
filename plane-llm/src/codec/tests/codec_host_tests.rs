// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Codecs read only the host supplied to them.

use crate::codec::cells::CodecHost;

/// A host whose entropy is one repeated byte and whose clock is one fixed second.
struct Fixed;

impl CodecHost for Fixed {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(0x2A);
        true
    }

    fn wall_clock_now(&self) -> Option<u64> {
        Some(1_700_000_000)
    }
}

/// A host with no entropy and no clock.
struct Silent;

impl CodecHost for Silent {
    fn fill_entropy(&self, _out: &mut [u8]) -> bool {
        false
    }

    fn wall_clock_now(&self) -> Option<u64> {
        None
    }
}

/// An answer with no id and no creation time, written as an OpenAI chat completion through `host`.
fn written(host: &dyn CodecHost) -> serde_json::Value {
    let writer = crate::codec::proto_codec::Protocol::openai();
    writer
        .writer()
        .write_response(host, &crate::codec::ir::IrResponse::default())
}

fn token(v: &serde_json::Value) -> String {
    let id = v["id"].as_str().expect("an id is always written");
    id.strip_prefix("chatcmpl-")
        .expect("the native id prefix")
        .to_string()
}

#[test]
fn the_minted_id_and_the_stamped_time_are_the_handed_hosts() {
    let v = written(&Fixed);
    let t = token(&v);
    let first = t.chars().next().expect("a non-empty token");
    assert!(
        t.chars().all(|c| c == first) && first != '0',
        "every byte of the token came from the handed host's entropy, got {t:?}"
    );
    assert_eq!(v["created"], 1_700_000_000, "the handed host's clock");
}

#[test]
fn unavailable_entropy_refuses_a_synthesized_response_identity() {
    use crate::codec::proto_codec::Protocol;
    for protocol in [
        Protocol::openai(),
        Protocol::responses(),
        Protocol::anthropic(),
    ] {
        let v = protocol
            .writer()
            .write_response(&Silent, &crate::codec::ir::IrResponse::default());
        assert!(v.get("error").is_some(), "{}: {v}", protocol.name_static());
        assert!(v.get("id").is_none(), "an error must not invent an id: {v}");
        let carried = crate::codec::ir::IrResponse {
            id: Some("upstream-carried-id".into()),
            ..Default::default()
        };
        let v = protocol.writer().write_response(&Silent, &carried);
        assert_eq!(v["id"], "upstream-carried-id");
    }
}

/// Entropy is available for the response id, then fails while minting its first output-item id.
struct OneDraw(std::sync::atomic::AtomicBool);

impl CodecHost for OneDraw {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        if self.0.swap(false, std::sync::atomic::Ordering::Relaxed) {
            out.fill(42);
            true
        } else {
            out.fill(7);
            false
        }
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}

fn chat_chunk(
    delta: serde_json::Value,
    finish: serde_json::Value,
    usage: serde_json::Value,
) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "upstream-id", "object": "chat.completion.chunk", "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}], "usage": usage
        })
    )
    .into_bytes()
}

#[test]
fn an_entropy_failure_terminates_translation_but_keeps_reported_usage() {
    for (host, ingress) in [
        (&Silent as &dyn CodecHost, "responses"),
        (&Silent as &dyn CodecHost, "anthropic"),
        (
            &OneDraw(std::sync::atomic::AtomicBool::new(true)) as &dyn CodecHost,
            "responses",
        ),
    ] {
        let mut stream =
            crate::codec::proto_stream::StreamTranslate::new(ingress, "openai").unwrap();
        let mut out = stream.feed(
            host,
            &chat_chunk(
                serde_json::json!({"role":"assistant","content":"unwritten-canary"}),
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
        );
        out.extend(stream.feed(
            host,
            &chat_chunk(
                serde_json::json!({}),
                serde_json::json!("stop"),
                serde_json::json!({"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}),
            ),
        ));
        out.extend(stream.finish(host));
        let text = String::from_utf8(out).unwrap();
        assert!(stream.terminal_error().is_some(), "{ingress}: {text}");
        assert!(
            text.contains("error"),
            "a client must receive a terminal error: {text}"
        );
        assert!(!text.contains("unwritten-canary"), "{text}");
        assert!(!text.contains("response.completed"), "{text}");
        let usage = stream
            .usage()
            .expect("executed usage survives delivery failure");
        assert_eq!((usage.input_tokens, usage.output_tokens), (7, 3));
    }
}

#[test]
fn buffered_entropy_failure_is_not_http_success_and_retains_executed_usage() {
    use crate::exchange::reply::whole::{self, WholeCtx, WholeEnd};
    let source = serde_json::json!({
        "id":"upstream-id", "object":"chat.completion", "model":"m",
        "choices":[{"index":0,"message":{"role":"assistant","content":"unwritten-canary"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
    });
    let anthropic_source = serde_json::json!({
        "id":"upstream-id", "type":"message", "model":"m", "role":"assistant",
        "content":[{"type":"text","text":"unwritten-canary"}], "stop_reason":"end_turn",
        "usage":{"input_tokens":7,"output_tokens":3}
    });
    for ingress in ["responses", "anthropic", "openai"] {
        let egress = if ingress == "openai" {
            "anthropic"
        } else {
            "openai"
        };
        let bytes = serde_json::to_vec(if ingress == "openai" {
            &anthropic_source
        } else {
            &source
        })
        .unwrap();
        for wants_stream in [false, true] {
            let ctx = WholeCtx {
                ingress,
                egress,
                operation: busbar_contract::operation::OpVerb::CHAT,
                model: "m",
                wants_stream,
                json_array: false,
                request: None,
                now_s: 1,
                elapsed_ms: None,
                translate_cap: crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
            };
            let good = whole::translate(&Fixed, &ctx, 200, &bytes);
            assert_eq!(good.end, WholeEnd::Delivered);
            let refused = whole::translate(&Silent, &ctx, 200, &bytes);
            assert_ne!(refused.end, WholeEnd::Delivered);
            assert!(refused.answer.status >= 500);
            assert_eq!(
                refused.usage, good.usage,
                "the provider already executed this work"
            );
            assert!(refused.usage.is_some());
            assert!(!String::from_utf8(refused.answer.body)
                .unwrap()
                .contains("unwritten-canary"));
        }
    }
}

#[test]
fn a_chat_stream_refuses_entropy_loss_and_retains_anthropic_usage() {
    let mut stream =
        crate::codec::proto_stream::StreamTranslate::new("openai", "anthropic").unwrap();
    let input = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"upstream-id\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let mut out = stream.feed(&Silent, input.as_bytes());
    out.extend(stream.finish(&Silent));
    let out = String::from_utf8(out).unwrap();
    assert!(stream.terminal_error().is_some(), "{out}");
    assert!(out.contains("error"), "{out}");
    assert!(!out.contains("chatcmpl-"), "{out}");
    let usage = stream.usage().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 3));
}

#[test]
fn request_identity_synthesis_failure_refuses_egress() {
    use crate::codec::cells::{EgressWire, OperationHandler};
    let request = serde_json::json!({"model":"m","max_tokens":16,"messages":[{
        "role":"assistant", "content":[{"type":"thinking","thinking":"reasoning-canary"}]
    }]});
    let mut handle = crate::codec::chat_handle::ChatOperation("anthropic")
        .read_request_value(&request)
        .unwrap();
    assert!(matches!(
        handle.write_egress_request(&Fixed, "responses", "m"),
        EgressWire::Json(_)
    ));
    assert!(matches!(
        handle.write_egress_request(&Silent, "responses", "m"),
        EgressWire::Unrepresentable { .. }
    ));
}

#[test]
fn an_error_without_usable_entropy_keeps_its_failure_without_inventing_an_id() {
    let protocol = crate::codec::proto_codec::Protocol::anthropic();
    let writer = protocol.writer();
    for entropy in [b"".as_slice(), &[255u8][..]] {
        let body =
            writer.write_error_from_entropy(&Silent, 500, "internal", "original failure", entropy);
        assert_eq!(body["error"]["message"], "original failure");
        assert!(body["request_id"].is_null(), "{body}");
    }
    let body = writer.write_error_from_entropy(&Fixed, 500, "internal", "original failure", &[42]);
    assert!(body["request_id"].as_str().unwrap().starts_with("req_01"));
}

#[test]
fn a_terminal_flush_entropy_failure_marks_the_real_relay_end_failed() {
    use crate::exchange::reply::relay::{Relay, RelayCtx};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountRefusals(AtomicUsize);
    impl CodecHost for CountRefusals {
        fn fill_entropy(&self, _out: &mut [u8]) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed);
            false
        }
        fn wall_clock_now(&self) -> Option<u64> {
            Some(1)
        }
    }
    static HANDLER: crate::codec::chat_handle::ChatOperation =
        crate::codec::chat_handle::ChatOperation("responses");
    let ctx = RelayCtx {
        ingress: "responses",
        egress: "anthropic",
        far_is_stream: true,
        json_array: false,
        client_include_usage: true,
        request: None,
        handler: &HANDLER,
        meter: true,
        translate_cap: crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };

    let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n";
    let host = CountRefusals(AtomicUsize::new(0));
    let mut relay = Relay::new(ctx);
    let _ = relay.feed(&host, delta);
    assert_eq!(
        host.0.load(Ordering::Relaxed),
        0,
        "entropy is not needed before terminal flush"
    );
    let end = relay.end(&host);
    assert!(host.0.load(Ordering::Relaxed) > 0);
    assert!(end.failed);
    assert!(end.stream_fault.is_some());
    let bytes = String::from_utf8(end.bytes).unwrap();
    assert!(bytes.contains("error"), "{bytes}");
    assert!(!bytes.contains("response.completed"), "{bytes}");
    let mut control = Relay::new(ctx);
    let _ = control.feed(&Fixed, delta);
    let control = control.end(&Fixed);
    assert!(!control.failed);
    assert!(end.usage.is_some());
    assert_eq!(end.usage, control.usage);
}

#[test]
fn upstream_failure_survives_failure_to_mint_its_error_correlation_id() {
    let mut stream =
        crate::codec::proto_stream::StreamTranslate::new("responses", "openai").unwrap();
    let out = stream.feed(&Silent, b"data: {\"error\":{\"type\":\"server_error\",\"code\":\"original-provider-failure\",\"message\":\"original-provider-failure\"}}\n\n");
    assert_eq!(stream.terminal_error(), Some("original-provider-failure"));
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("original-provider-failure"));
}
