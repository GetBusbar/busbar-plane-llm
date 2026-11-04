// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use crate::codec::ir::{IrBlock, IrResponse, IrRole, IrStopReason, IrUsage, StreamDecodeState};
use busbar_contract::codec::CodecHost;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Entropy {
    available: bool,
    draws: AtomicUsize,
}

impl Entropy {
    fn new(available: bool) -> Self {
        Self {
            available,
            draws: AtomicUsize::new(0),
        }
    }
}

impl CodecHost for Entropy {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        self.draws.fetch_add(1, Ordering::Relaxed);
        out.fill(if self.available { 42 } else { 7 });
        self.available
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}

fn usage() -> Value {
    json!({"tokens":{"input_tokens":17,"output_tokens":9},
        "billed_units":{"input_tokens":7,"output_tokens":3,"search_units":2,"classifications":1}})
}

fn cohere_body() -> Value {
    json!({"finish_reason":"COMPLETE", "message":{"role":"assistant",
        "content":[{"type":"text","text":"unwritten-answer-canary"}]}, "usage":usage()})
}

fn start(id: Option<&str>) -> IrStreamEvent {
    IrStreamEvent::MessageStart {
        role: IrRole::Assistant,
        usage: None,
        id: id.map(str::to_owned),
        created: None,
        model: None,
    }
}

#[test]
fn reading_an_absent_identity_preserves_usage_without_needing_entropy() {
    let host = Entropy::new(false);
    let response = CohereReader.read_response(&host, &cohere_body()).unwrap();
    assert_eq!(response.id, None);
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens),
        (17, 9)
    );
    assert_eq!(response.usage.detail.billed_input_tokens, Some(7));
    let mut state = StreamDecodeState::default();
    let events =
        CohereReader.read_response_events(&host, "", &json!({"type":"message-start"}), &mut state);
    assert!(matches!(
        &events[..],
        [IrStreamEvent::MessageStart { id: None, .. }]
    ));
    assert_eq!(host.draws.load(Ordering::Relaxed), 0);
}

#[test]
fn unavailable_entropy_never_becomes_a_buffered_or_streamed_identity() {
    for id in [None, Some("")] {
        let host = Entropy::new(false);
        let writer = CohereWriter;
        let response = IrResponse {
            id: id.map(str::to_owned),
            ..Default::default()
        };
        let body = writer.write_response(&host, &response);
        assert!(body.get("id").is_none(), "{body}");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|s| s.contains("entropy")),
            "{body}"
        );
        let (_, frame) = writer
            .write_response_event(&host, &start(id))
            .expect("failure is visible");
        assert!(frame.get("id").is_none(), "{frame}");
        assert!(
            frame["message"]
                .as_str()
                .is_some_and(|s| s.contains("entropy")),
            "{frame}"
        );
    }
}

#[test]
fn a_valid_stream_identity_is_minted_once_or_carried_without_entropy() {
    let host = Entropy::new(true);
    let writer = CohereWriter;
    let (_, first) = writer.write_response_event(&host, &start(None)).unwrap();
    let id = first["id"].as_str().unwrap();
    assert_eq!(
        id.split('-').map(str::len).collect::<Vec<_>>(),
        [8, 4, 4, 4, 12]
    );
    assert_eq!(id.as_bytes()[14], b'4');
    assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    assert!(id.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()));
    let unavailable = Entropy::new(false);
    for duplicate in [start(None), start(Some("another-upstream-id"))] {
        let (_, repeated) = writer
            .write_response_event(&unavailable, &duplicate)
            .unwrap();
        assert_eq!(repeated["id"], first["id"]);
    }
    assert_eq!(host.draws.load(Ordering::Relaxed), 1);
    assert_eq!(unavailable.draws.load(Ordering::Relaxed), 0);
    let carried = CohereWriter;
    let (_, frame) = carried
        .write_response_event(&unavailable, &start(Some(id)))
        .unwrap();
    assert_eq!(frame["id"], first["id"]);
    let body = carried.write_response(
        &unavailable,
        &IrResponse {
            id: Some(id.into()),
            ..Default::default()
        },
    );
    assert_eq!(body["id"], first["id"]);
    assert_eq!(unavailable.draws.load(Ordering::Relaxed), 0);
}

#[test]
fn whole_delivery_failure_keeps_reported_usage_in_both_cohere_directions() {
    use crate::exchange::reply::whole::{self, WholeCtx, WholeEnd};
    use busbar_contract::billing::Billing;
    for egress in ["cohere", "openai"] {
        let source = if egress == "cohere" {
            cohere_body()
        } else {
            json!({
                "id":"upstream-id", "model":"m", "object":"chat.completion",
                "choices":[{"index":0,"message":{"role":"assistant","content":"unwritten-answer-canary"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":17,"completion_tokens":9,"total_tokens":26}
            })
        };
        for ingress in ["cohere", "openai"] {
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
                    translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
                };
                let bytes = serde_json::to_vec(&source).unwrap();
                let good = whole::translate(&Entropy::new(true), &ctx, 200, &bytes);
                assert_eq!(
                    good.end,
                    WholeEnd::Delivered,
                    "{ingress}/{egress}/{wants_stream}: {good:?}"
                );
                let failed = whole::translate(&Entropy::new(false), &ctx, 200, &bytes);
                assert_ne!(
                    failed.end,
                    WholeEnd::Delivered,
                    "{ingress}/{egress}/{wants_stream}: {failed:?}"
                );
                assert!(failed.answer.status >= 500);
                assert_eq!(failed.usage, good.usage, "provider work remains accounted");
                let Some(Billing::Tokens(tokens)) = failed.usage else {
                    panic!("reported tokens lost")
                };
                assert_eq!((tokens.input, tokens.output), (17, 9));
                if egress == "cohere" {
                    assert_eq!(tokens.open_units.get("search_units"), Some(&2));
                    assert_eq!(tokens.open_units.get("classifications"), Some(&1));
                } else {
                    assert!(tokens.open_units.is_empty());
                }
                assert!(!String::from_utf8(failed.answer.body)
                    .unwrap()
                    .contains("unwritten-answer-canary"));
                let mut absent = source.clone();
                absent.as_object_mut().unwrap().remove("usage");
                let absent = whole::translate(
                    &Entropy::new(false),
                    &ctx,
                    200,
                    &serde_json::to_vec(&absent).unwrap(),
                );
                let Some(Billing::Tokens(tokens)) = absent.usage else {
                    panic!("zero usage projection missing")
                };
                assert_eq!((tokens.input, tokens.output), (0, 0));
                assert!(
                    tokens.open_units.is_empty(),
                    "no unreported units are invented"
                );
            }
        }
    }
}

fn sse(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

#[test]
fn live_entropy_failure_keeps_terminal_usage_in_both_cohere_directions() {
    for (ingress, egress) in [("cohere", "openai"), ("openai", "cohere")] {
        let frames = if egress == "cohere" {
            vec![
                sse(json!({"type":"message-start"})),
                sse(
                    json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"unwritten-answer-canary"}}}}),
                ),
                sse(
                    json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":usage()}}),
                ),
            ]
        } else {
            vec![
                sse(
                    json!({"id":"upstream-id","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"unwritten-answer-canary"},"finish_reason":null}]}),
                ),
                sse(
                    json!({"id":"upstream-id","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":9,"total_tokens":26}}),
                ),
            ]
        };
        let mut stream = crate::codec::proto_stream::StreamTranslate::new(ingress, egress).unwrap();
        let host = Entropy::new(false);
        let mut out = Vec::new();
        for frame in frames {
            out.extend(stream.feed(&host, &frame));
        }
        out.extend(stream.finish(&host));
        let text = String::from_utf8(out).unwrap();
        assert!(
            stream.terminal_error().is_some(),
            "{ingress}/{egress}: {text}"
        );
        assert!(text.contains("error") || text.contains("ERROR"), "{text}");
        assert!(!text.contains("unwritten-answer-canary"), "{text}");
        let tokens = stream
            .usage()
            .expect("terminal provider usage survives")
            .to_token_usage();
        assert_eq!((tokens.input, tokens.output), (17, 9));
        if egress == "cohere" {
            assert_eq!(tokens.open_units.get("search_units"), Some(&2));
            assert_eq!(tokens.open_units.get("classifications"), Some(&1));
        } else {
            assert!(tokens.open_units.is_empty());
        }
    }
}

#[test]
fn mixed_tool_result_parts_keep_their_original_positions() {
    let body = json!({"model":"m","messages":[{"role":"tool","tool_call_id":"call_1","content":[
        {"type":"text","text":"A"},
        {"type":"document","document":{"id":"d1","data":{"text":"B"}}},
        {"type":"text","text":"C"},
        {"type":"document","document":{"id":"d2","data":"D"}},
        "E"
    ]}]});
    let ir = CohereReader.read_request(&body).unwrap();
    let IrBlock::ToolResult { content, .. } = &ir.messages[0].content[0] else {
        panic!("tool result missing")
    };
    assert_eq!(
        content,
        &vec![
            IrBlock::text("A"),
            IrBlock::Json(json!({"text":"B"})),
            IrBlock::text("C"),
            IrBlock::text("D"),
            IrBlock::text("E")
        ]
    );
}

#[test]
fn genuine_errors_use_v2_error_even_when_the_signal_mentions_safety() {
    for signal in [
        Some("content_filter_safety"),
        Some("upstream unavailable"),
        None,
    ] {
        let writer = CohereWriter;
        let event = IrStreamEvent::Error(IrError {
            class: StatusClass::ServerError,
            provider_signal: signal.map(str::to_owned),
            retry_after: None,
        });
        let (_, frame) = writer
            .write_response_event(crate::codec::test_host::HOST, &event)
            .unwrap();
        assert_eq!(frame["type"], "message-end");
        assert_eq!(frame["delta"]["finish_reason"], "ERROR");
        let decoded = CohereReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &frame,
            &mut StreamDecodeState::default(),
        );
        assert!(
            decoded
                .iter()
                .any(|event| matches!(event, IrStreamEvent::Error(_))),
            "{decoded:?}"
        );
    }
    let writer = CohereWriter;
    let (_, safe) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(IrStopReason::Safety),
                stop_sequence: None,
                usage: IrUsage::default(),
                stop_detail: None,
            },
        )
        .unwrap();
    assert_eq!(safe["delta"]["finish_reason"], "ERROR");
}
