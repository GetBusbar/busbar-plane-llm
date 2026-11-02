// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! A native error marker must remain a failure even without its nested error object.

use busbar_contract::abi::transport::FAULT_TRANSIENT;
use busbar_contract::codec::CodecHost;
use busbar_contract::operation::OpVerb;
use busbar_contract::upstream::StatusClass;
use busbar_plane_llm::codec::ir::{IrStreamEvent, StreamDecodeState};
use busbar_plane_llm::codec::proto_codec::protocol_for;
use busbar_plane_llm::codec::proto_stream::StreamTranslate;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Fault, Reply, ReplyCtx, Units, Verdict};
use busbar_plane_llm::exchange::shaping::Lane;
use busbar_plane_llm::plane_door::breaker_fault;
use serde_json::{json, Value};

struct Host;
impl CodecHost for Host {
    fn fill_entropy(&self, bytes: &mut [u8]) -> bool {
        bytes.fill(7);
        true
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}

const MARKER: &[u8] = b"event: error\r\ndata: { \"type\": \"error\" }\r\n\r\n";

fn frame(event: &str, data: Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn stream(ingress: &str) -> StreamTranslate {
    if ingress == "anthropic" {
        StreamTranslate::new_same_proto("anthropic")
    } else {
        StreamTranslate::new(ingress, "anthropic")
    }
    .unwrap()
}

fn assert_error(event: IrStreamEvent, class: StatusClass, signal: Option<&str>) {
    let IrStreamEvent::Error(error) = event else {
        panic!("expected only Error IR, got {event:?}");
    };
    assert_eq!(error.class, class);
    assert_eq!(error.provider_signal.as_deref(), signal);
    assert_eq!(error.retry_after, None);
}

#[test]
fn singular_reader_retains_the_missing_container_error_marker() {
    let protocol = protocol_for("anthropic").unwrap();
    let event = protocol
        .reader()
        .read_response_event(&Host, "error", &json!({"type":"error"}));
    assert_error(
        event.expect("recognized error marker is a failure"),
        StatusClass::ServerError,
        None,
    );
}

#[test]
fn plural_reader_retains_exactly_one_error_without_synthetic_content() {
    let events = protocol_for("anthropic")
        .unwrap()
        .reader()
        .read_response_events(
            &Host,
            "error",
            &json!({"type":"error"}),
            &mut StreamDecodeState::default(),
        );
    assert_eq!(events.len(), 1, "recognized error marker is a failure");
    assert_error(
        events.into_iter().next().unwrap(),
        StatusClass::ServerError,
        None,
    );
}

#[test]
fn native_error_bytes_survive_every_split_while_terminal_failure_is_recorded() {
    for split in 0..=MARKER.len() {
        let mut translate = stream("anthropic");
        let mut output = translate.feed(&Host, &MARKER[..split]);
        output.extend(translate.feed(&Host, &MARKER[split..]));
        output.extend(translate.finish(&Host));
        assert_eq!(output, MARKER, "split {split}: native bytes stay exact");
        assert_eq!(
            translate.terminal_error(),
            Some("upstream stream error"),
            "split {split}"
        );
    }
}

fn content_frames() -> Vec<Vec<u8>> {
    vec![
        frame(
            "message_start",
            json!({"type":"message_start", "message":{
                "id":"msg-marker", "model":"model", "role":"assistant", "usage":{
                    "input_tokens":17,"cache_read_input_tokens":4,"cache_creation_input_tokens":3
                }
            }}),
        ),
        frame(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":""}}),
        ),
        frame(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"delivered-text"}}),
        ),
        frame(
            "message_delta",
            json!({"delta":{},"usage":{"output_tokens":9,
            "server_tool_use":{"web_search_requests":2,"web_fetch_requests":1}}}),
        ),
    ]
}

fn check_translated_failure(ingress: &str) {
    let mut translate = stream(ingress);
    let mut delivered = Vec::new();
    for bytes in content_frames() {
        delivered.extend(translate.feed(&Host, &bytes));
    }
    assert!(String::from_utf8_lossy(&delivered).contains("delivered-text"));
    let mut failed = translate.feed(&Host, MARKER);
    failed.extend(translate.feed(
        &Host,
        &frame(
            "content_block_delta",
            json!({
                "index":0,"delta":{"type":"text_delta","text":"must-not-resume"}
            }),
        ),
    ));
    failed.extend(translate.feed(
        &Host,
        &frame(
            "message_delta",
            json!({
                "delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}
            }),
        ),
    ));
    failed.extend(translate.feed(&Host, &frame("message_stop", json!({}))));
    failed.extend(translate.finish(&Host));
    assert_eq!(translate.terminal_error(), Some("upstream stream error"));
    let wire = String::from_utf8_lossy(&failed);
    assert!(wire.contains("\"error\""), "existing error shape: {wire}");
    assert!(!wire.contains("must-not-resume"), "{wire}");
    assert!(!wire.contains("\"finish_reason\":\"stop\""), "{wire}");
    assert!(!wire.contains("finishReason"), "{wire}");
}

#[test]
fn openai_translation_keeps_the_error_and_cannot_resume_or_finish_successfully() {
    check_translated_failure("openai");
}

#[test]
fn gemini_translation_keeps_the_error_and_cannot_resume_or_finish_successfully() {
    check_translated_failure("gemini");
}

#[test]
fn missing_container_error_discards_deferred_tool_content() {
    let mut translate = stream("gemini");
    for bytes in content_frames() {
        translate.feed(&Host, &bytes);
    }
    translate.feed(&Host, &frame("content_block_stop", json!({"index":0})));
    let mut tool = translate.feed(
        &Host,
        &frame(
            "content_block_start",
            json!({
                "index":1,"content_block":{"type":"tool_use","id":"call-marker","name":"send"}
            }),
        ),
    );
    tool.extend(translate.feed(
        &Host,
        &frame(
            "content_block_delta",
            json!({
                "index":1,"delta":{"type":"input_json_delta","partial_json":"{\"value\":1}"}
            }),
        ),
    ));
    tool.extend(translate.feed(&Host, MARKER));
    tool.extend(translate.finish(&Host));
    assert_eq!(translate.terminal_error(), Some("upstream stream error"));
    let wire = String::from_utf8_lossy(&tool);
    assert!(wire.contains("\"error\""), "{wire}");
    assert!(
        !wire.contains("functionCall"),
        "deferred tool must not execute: {wire}"
    );
    assert!(!wire.contains("finishReason"), "{wire}");
}

struct Context {
    arrived: Arrived,
    lane: Lane,
}
impl Context {
    fn new(ingress: &'static str) -> Self {
        let request = json!({"model":"model","stream":true});
        Self {
            arrived: Arrived {
                dialect: ingress,
                operation: OpVerb::CHAT,
                model: "model".into(),
                content_type: "application/json".into(),
                body: serde_json::to_vec(&request).unwrap(),
                parsed: Some(request),
                path: "/v1/chat/completions".into(),
                query: None,
                path_model: None,
            },
            lane: Lane {
                model: "model".into(),
                provider: "provider".into(),
                dialect: "anthropic",
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
            },
        }
    }
    fn borrow(&self) -> ReplyCtx<'_> {
        ReplyCtx {
            arrived: &self.arrived,
            lane: &self.lane,
            intent: StreamIntent {
                wants_stream: true,
                client_include_usage: true,
            },
            passthrough: false,
            translate_cap: 1024 * 1024,
        }
    }
}

fn check_reply(ingress: &'static str) {
    let context = Context::new(ingress);
    let ctx = context.borrow();
    let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"text/event-stream")]);
    let mut delivered = Vec::new();
    let mut probe = stream(ingress);
    for bytes in content_frames() {
        let piece = reply.feed(&Host, &ctx, &bytes, false, At::default());
        delivered.extend_from_slice(&piece.bytes);
        probe.feed(&Host, &bytes);
    }
    assert!(String::from_utf8_lossy(&delivered).contains("delivered-text"));
    let usage = probe
        .usage()
        .expect("establish real reported usage before failure")
        .to_token_usage();
    assert_eq!((usage.input, usage.output), (17, 9));
    assert_eq!((usage.cache_read, usage.cache_creation), (Some(4), Some(3)));
    assert_eq!(usage.open_units.get("search_units"), Some(&2));
    assert_eq!(usage.open_units.get("web_fetch_requests"), Some(&1));
    reply.feed(&Host, &ctx, MARKER, false, At::default());
    let piece = reply.feed(&Host, &ctx, b"", true, At::default());
    assert_eq!(piece.verdict, Verdict::Hard, "{ingress}: {piece:?}");
    assert_eq!(
        piece.fault,
        Some(Fault::Transient("stream-terminal-error")),
        "{ingress}: {piece:?}"
    );
    assert_eq!(breaker_fault(piece.fault.as_ref()), FAULT_TRANSIENT);
    assert_eq!(
        piece.units,
        Units::default(),
        "existing upstream-error refund: {ingress}"
    );
    assert!(piece.done);
    assert!(
        !reply.partial(),
        "a terminal error is not a successful partial reply"
    );
}

#[test]
fn native_actual_reply_refunds_known_work_and_reports_the_actual_breaker_fault() {
    check_reply("anthropic");
}

#[test]
fn openai_actual_reply_refunds_known_work_and_reports_the_actual_breaker_fault() {
    check_reply("openai");
}

#[test]
fn gemini_actual_reply_refunds_known_work_and_reports_the_actual_breaker_fault() {
    check_reply("gemini");
}

#[test]
fn all_existing_named_error_classes_and_signals_are_preserved() {
    for (signal, class) in [
        ("api_error", StatusClass::ServerError),
        ("overloaded_error", StatusClass::Overloaded),
        ("rate_limit_error", StatusClass::RateLimit),
        ("timeout_error", StatusClass::Timeout),
        ("authentication_error", StatusClass::Auth),
        ("permission_error", StatusClass::Auth),
        ("billing_error", StatusClass::Billing),
        ("invalid_request_error", StatusClass::ClientError),
        ("not_found_error", StatusClass::ClientError),
        ("request_too_large", StatusClass::ClientError),
        ("unknown_error", StatusClass::ClientError),
    ] {
        let event = protocol_for("anthropic")
            .unwrap()
            .reader()
            .read_response_event(
                &Host,
                "error",
                &json!({"type":"error","error":{"type":signal}}),
            )
            .unwrap();
        assert_error(event, class, Some(signal));
    }
}

#[test]
fn present_null_empty_scalar_and_typeless_containers_keep_their_existing_mapping() {
    for container in [
        Value::Null,
        json!({}),
        json!(7),
        json!("scalar"),
        json!([]),
        json!({"message":"no type"}),
        json!({"type":7}),
        json!({"type":null}),
    ] {
        let event = protocol_for("anthropic")
            .unwrap()
            .reader()
            .read_response_event(&Host, "error", &json!({"type":"error","error":container}))
            .unwrap();
        assert_error(event, StatusClass::ClientError, None);
    }
}

#[test]
fn unknown_ping_and_unnamed_events_do_not_invent_failures_from_body_tokens() {
    for event_type in ["unknown-event", "ping", ""] {
        let data = json!({"type":"error","error":{"type":"api_error"}});
        let protocol = protocol_for("anthropic").unwrap();
        assert!(protocol
            .reader()
            .read_response_event(&Host, event_type, &data)
            .is_none());
        assert!(protocol
            .reader()
            .read_response_events(&Host, event_type, &data, &mut StreamDecodeState::default(),)
            .is_empty());
        let mut translate = stream("anthropic");
        let native = frame(event_type, data);
        let mut bytes = translate.feed(&Host, &native);
        bytes.extend(translate.finish(&Host));
        assert_eq!(bytes, native);
        assert_eq!(translate.terminal_error(), None);
    }
}
