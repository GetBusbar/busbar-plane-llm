//! Native reported work must survive unrelated Anthropic content-shape failures.

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::codec::cells::CodecHost;
use busbar_plane_llm::codec::ir::{IrStreamEvent, StreamDecodeState};
use busbar_plane_llm::codec::proto_codec::protocol_for;
use busbar_plane_llm::codec::proto_stream::StreamTranslate;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Fault, Piece, Reply, ReplyCtx, Verdict};
use busbar_plane_llm::exchange::shaping::Lane;
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

fn stream(ingress: &str) -> StreamTranslate {
    let mut stream = if ingress == "anthropic" {
        StreamTranslate::new_same_proto("anthropic")
    } else {
        StreamTranslate::new(ingress, "anthropic")
    }
    .unwrap();
    stream.set_client_include_usage(true);
    stream
}

fn frame(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn start(role: Option<Value>, usage: Value) -> Value {
    let mut message = json!({"id":"msg-native", "model":"model", "usage":usage});
    if let Some(role) = role {
        message["role"] = role;
    }
    json!({"type":"message_start", "message":message})
}

fn malformed_roles() -> [Option<Value>; 3] {
    [None, Some(json!(7)), Some(json!("unrecognized-role"))]
}

fn counts(stream: &StreamTranslate) -> Option<(u64, u64)> {
    stream.usage().map(|u| (u.input_tokens, u.output_tokens))
}

fn reader_events(event: &str, data: &Value) -> Vec<IrStreamEvent> {
    protocol_for("anthropic")
        .unwrap()
        .reader()
        .read_response_events(&Host, event, data, &mut StreamDecodeState::default())
}

fn check_start_recovery(ingress: &str) {
    for role in malformed_roles() {
        let data = start(role, json!({"input_tokens":5000}));
        assert!(reader_events("message_start", &data).is_empty());
        let native = frame("message_start", &data);
        let mut translated = stream(ingress);
        let output = translated.feed(&Host, &native);
        assert_eq!(counts(&translated), Some((5000, 0)), "{ingress}: {data}");
        if ingress == "anthropic" {
            assert_eq!(output, native, "native malformed content remains verbatim");
        } else {
            assert!(
                output.is_empty(),
                "accounting must not synthesize content: {output:?}"
            );
        }
    }
}

#[test]
fn native_start_usage_survives_missing_and_unrecognized_role() {
    check_start_recovery("anthropic");
}

#[test]
fn translated_start_usage_survives_missing_and_unrecognized_role() {
    for ingress in ["openai", "gemini"] {
        check_start_recovery(ingress);
    }
}

fn check_trailing_recovery(ingress: &str) {
    let first = frame(
        "message_start",
        &start(Some(json!("assistant")), json!({"input_tokens":5000})),
    );
    let data = json!({"type":"message_delta", "usage":{"output_tokens":812}});
    assert!(reader_events("message_delta", &data).is_empty());
    let last = frame("message_delta", &data);
    let mut translated = stream(ingress);
    translated.feed(&Host, &first);
    let output = translated.feed(&Host, &last);
    assert_eq!(counts(&translated), Some((5000, 812)), "{ingress}");
    if ingress == "anthropic" {
        assert_eq!(output, last);
    } else {
        assert!(
            output.is_empty(),
            "accounting must not synthesize a terminal event"
        );
    }
}

#[test]
fn native_trailing_usage_survives_absent_delta() {
    check_trailing_recovery("anthropic");
}

#[test]
fn translated_trailing_usage_survives_absent_delta() {
    for ingress in ["openai", "gemini"] {
        check_trailing_recovery(ingress);
    }
}

fn json_frames(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .split("\n\n")
        .filter_map(|raw| {
            let (_, data) = busbar_plane_llm::codec::dialect::parse_sse_frame(raw.as_bytes())?;
            serde_json::from_str(&data).ok()
        })
        .collect()
}

#[test]
fn a_recovered_start_backfills_the_next_real_terminal_event() {
    for ingress in ["openai", "gemini"] {
        let mut translated = stream(ingress);
        translated.feed(&Host, &frame("message_start", &start(None, json!({
            "input_tokens":5000, "cache_read_input_tokens":42,
            "cache_creation_input_tokens":17,
            "cache_creation":{"ephemeral_5m_input_tokens":17,"ephemeral_1h_input_tokens":0}
        }))));
        let mut output = translated.feed(
            &Host,
            &frame(
                "message_delta",
                &json!({
                    "type":"message_delta", "delta":{"stop_reason":"end_turn"},
                    "usage":{"output_tokens":812}
                }),
            ),
        );
        output.extend(translated.finish(&Host));
        assert_eq!(counts(&translated), Some((5000, 812)), "{ingress}");
        let usage = translated.usage().unwrap();
        assert_eq!(usage.cache_read_input_tokens, Some(42));
        assert_eq!(usage.cache_creation_input_tokens, Some(17));
        assert_eq!(usage.detail.cache_creation_5m_input_tokens, Some(17));

        let mut ordinary = stream(ingress);
        ordinary.feed(&Host, &frame("message_start", &start(Some(json!("assistant")), json!({
            "input_tokens":5000, "cache_read_input_tokens":42,
            "cache_creation_input_tokens":17,
            "cache_creation":{"ephemeral_5m_input_tokens":17,"ephemeral_1h_input_tokens":0}
        }))));
        let mut expected = ordinary.feed(
            &Host,
            &frame(
                "message_delta",
                &json!({
                    "type":"message_delta", "delta":{"stop_reason":"end_turn"},
                    "usage":{"output_tokens":812}
                }),
            ),
        );
        expected.extend(ordinary.finish(&Host));
        let key = if ingress == "openai" {
            "usage"
        } else {
            "usageMetadata"
        };
        let usage_frames = |bytes: &[u8]| {
            json_frames(bytes)
                .into_iter()
                .filter_map(|value| value.get(key).cloned())
                .collect::<Vec<_>>()
        };
        let expected = usage_frames(&expected);
        assert!(
            !expected.is_empty(),
            "the existing writer must emit terminal usage"
        );
        let prompt_key = if ingress == "openai" {
            "prompt_tokens"
        } else {
            "promptTokenCount"
        };
        assert!(expected
            .iter()
            .any(|usage| usage[prompt_key] == json!(5059)));
        assert_eq!(
            usage_frames(&output),
            expected,
            "the recovered start must produce exactly the existing terminal usage: {ingress}"
        );
    }
}

#[test]
fn native_bytes_and_reported_counts_survive_every_frame_split() {
    let native = b"event: message_start\r\ndata: { \"type\": \"message_start\", \"message\": {\"usage\":{\"input_tokens\":5000}} }\r\n\r\nevent: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\nevent: message_delta\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":812}}\r\n\r\n";
    for split in 0..=native.len() {
        let mut translated = stream("anthropic");
        let mut output = translated.feed(&Host, &native[..split]);
        output.extend(translated.feed(&Host, &native[split..]));
        output.extend(translated.finish(&Host));
        assert_eq!(output, native, "split {split}");
        assert_eq!(counts(&translated), Some((5000, 812)), "split {split}");
    }
}

#[test]
fn refused_counts_keep_readable_siblings_without_inventing_units() {
    for ingress in ["anthropic", "openai", "gemini"] {
        for bad in [
            json!("5000"),
            json!(-1),
            json!(1.5),
            json!(9_007_199_254_740_992.0),
        ] {
            let mut translated = stream(ingress);
            translated.feed(
                &Host,
                &frame(
                    "message_start",
                    &start(
                        None,
                        json!({
                            "input_tokens":bad, "output_tokens":812
                        }),
                    ),
                ),
            );
            assert_eq!(counts(&translated), Some((0, 812)), "{ingress}: {bad}");
        }
        let mut translated = stream(ingress);
        translated.feed(
            &Host,
            &frame(
                "message_start",
                &start(
                    None,
                    json!({
                        "input_tokens":27.0, "output_tokens":0
                    }),
                ),
            ),
        );
        assert_eq!(counts(&translated), Some((27, 0)), "{ingress}");
        translated.feed(
            &Host,
            &frame(
                "message_delta",
                &json!({
                    "usage":{"output_tokens":"812", "cache_read_input_tokens":42}
                }),
            ),
        );
        let usage = translated.usage().unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (27, 0));
        assert_eq!(usage.cache_read_input_tokens, Some(42));
    }
}

#[test]
fn unrelated_and_absent_usage_cannot_establish_reported_work() {
    let cases = [
        ("message_start", json!({"message":{}})),
        ("message_start", json!({"message":{"usage":null}})),
        ("message_start", json!({"message":{"usage":"5000"}})),
        (
            "message_start",
            json!({"message":{"content":{"usage":{"input_tokens":5000}}}}),
        ),
        ("message_delta", json!({})),
        ("message_delta", json!({"usage":[]})),
        (
            "message_delta",
            json!({"extra":{"usage":{"output_tokens":812}}}),
        ),
        (
            "unknown_event",
            json!({"usage":{"input_tokens":5000,"output_tokens":812}}),
        ),
    ];
    for ingress in ["anthropic", "openai", "gemini"] {
        for (event, data) in &cases {
            let mut translated = stream(ingress);
            translated.feed(&Host, &frame(event, data));
            assert_eq!(counts(&translated), None, "{ingress}: {event}: {data}");
        }
    }
}

#[test]
fn ordinary_metadata_and_repeated_usage_keep_the_existing_cumulative_contract() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let mut translated = stream(ingress);
        translated.feed(
            &Host,
            &frame(
                "message_start",
                &start(
                    Some(json!("assistant")),
                    json!({
                        "input_tokens":5000, "cache_creation_input_tokens":17,
                        "server_tool_use":{"web_search_requests":3,"web_fetch_requests":2}
                    }),
                ),
            ),
        );
        for _ in 0..2 {
            translated.feed(
                &Host,
                &frame(
                    "message_delta",
                    &json!({
                        "delta":{}, "usage":{"output_tokens":812,
                            "server_tool_use":{"web_search_requests":3,"web_fetch_requests":2}}
                    }),
                ),
            );
        }
        let usage = translated.usage().unwrap().to_token_usage();
        assert_eq!((usage.input, usage.output), (5000, 812), "{ingress}");
        assert_eq!(usage.cache_creation, Some(17));
        assert_eq!(usage.open_units.get("search_units"), Some(&3));
        assert_eq!(usage.open_units.get("web_fetch_requests"), Some(&2));
        let mut absent = stream(ingress);
        absent.feed(&Host, &frame("message_delta", &json!({"delta":{}})));
        assert_eq!(
            counts(&absent),
            Some((0, 0)),
            "existing valid metadata still reads absent counts as zero"
        );
    }
}

struct UsageWarnings(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl tracing::Subscriber for UsageWarnings {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target().ends_with("::codec::usage_count") {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn an_existing_usage_bearing_ir_event_refuses_and_warns_each_bad_count_only_once() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let warnings = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tracing::subscriber::with_default(UsageWarnings(warnings.clone()), || {
            let mut translated = stream(ingress);
            translated.feed(
                &Host,
                &frame(
                    "message_start",
                    &start(
                        Some(json!("assistant")),
                        json!({
                            "input_tokens":"unreadable", "output_tokens":9
                        }),
                    ),
                ),
            );
            assert_eq!(counts(&translated), Some((0, 9)));
        });
        assert_eq!(
            warnings.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "{ingress}: the fallback must not reread an accepted usage-bearing IR frame"
        );
    }
}

#[test]
fn recovered_detail_and_open_counts_keep_the_same_cumulative_precedence() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let mut translated = stream(ingress);
        translated.feed(&Host, &frame("message_start", &start(None, json!({
            "input_tokens":5000, "cache_creation_input_tokens":17,"cache_read_input_tokens":42,
            "cache_creation":{"ephemeral_5m_input_tokens":17,"ephemeral_1h_input_tokens":0},
            "service_tier":"batch"
        }))));
        for _ in 0..2 {
            translated.feed(
                &Host,
                &frame(
                    "message_delta",
                    &json!({"usage":{
                        "output_tokens":812,"output_tokens_details":{"thinking_tokens":27},
                        "server_tool_use":{"web_search_requests":3,"web_fetch_requests":2}
                    }}),
                ),
            );
        }
        let usage = translated.usage().unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (5000, 812));
        assert_eq!(usage.detail.cache_creation_5m_input_tokens, Some(17));
        assert_eq!(usage.detail.cache_creation_1h_input_tokens, Some(0));
        assert_eq!(usage.detail.reasoning_tokens, Some(27));
        assert_eq!(usage.detail.service_tier.as_deref(), Some("batch"));
        let billed = usage.to_token_usage();
        assert_eq!(billed.cache_creation, Some(17));
        assert_eq!(billed.cache_read, Some(42));
        assert_eq!(billed.open_units.get("search_units"), Some(&3));
        assert_eq!(billed.open_units.get("web_fetch_requests"), Some(&2));
    }
}

#[test]
fn malformed_optional_counts_do_not_erase_valid_recovered_siblings() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let mut translated = stream(ingress);
        translated.feed(
            &Host,
            &frame(
                "message_start",
                &start(
                    None,
                    json!({
                        "input_tokens":5000.0,"output_tokens":812.0,
                        "cache_creation_input_tokens":"bad", "cache_read_input_tokens":42,
                        "server_tool_use":{"web_search_requests":"bad","web_fetch_requests":3}
                    }),
                ),
            ),
        );
        let usage = translated.usage().unwrap().to_token_usage();
        assert_eq!((usage.input, usage.output), (5000, 812));
        assert_eq!(usage.cache_creation, None);
        assert_eq!(usage.cache_read, Some(42));
        assert!(!usage.open_units.contains_key("search_units"));
        assert_eq!(usage.open_units.get("web_fetch_requests"), Some(&3));
    }
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

fn content_frames() -> Vec<Vec<u8>> {
    vec![
        frame(
            "message_start",
            &start(
                None,
                json!({"input_tokens":5000,"cache_creation_input_tokens":17,"cache_read_input_tokens":42,"server_tool_use":{"web_search_requests":3,"web_fetch_requests":2}}),
            ),
        ),
        frame(
            "content_block_start",
            &json!({"index":0,"content_block":{"type":"text","text":""}}),
        ),
        frame(
            "content_block_delta",
            &json!({"index":0,"delta":{"type":"text_delta","text":"hello"}}),
        ),
        frame("content_block_stop", &json!({"index":0})),
        frame("message_delta", &json!({"usage":{"output_tokens":812}})),
    ]
}

#[test]
fn actual_reply_transport_failure_settles_the_reported_start_and_tail() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let context = Context::new(ingress);
        let ctx = context.borrow();
        let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"text/event-stream")]);
        let native = content_frames();
        let mut delivered = Vec::new();
        for bytes in &native {
            let piece = reply.feed(&Host, &ctx, bytes, false, At::default());
            delivered.extend_from_slice(&piece.bytes);
        }
        assert!(
            !delivered.is_empty(),
            "{ingress} must have delivered bytes before the cut"
        );
        if ingress == "anthropic" {
            assert_eq!(delivered, native.concat());
        }
        let cut = reply.cut(&Host, &ctx, true);
        assert_eq!(
            (cut.units.tokens_in, cut.units.tokens_out),
            (5000, 812),
            "{ingress}: {cut:?}"
        );
        assert_eq!(cut.units.cache_write, 17);
        assert_eq!(cut.units.cache_read, 42);
        assert_eq!(cut.units.open.get("search_units"), Some(&3));
        assert_eq!(cut.units.open.get("web_fetch_requests"), Some(&2));
        assert!(!cut.units.floor);
        assert_eq!(cut.verdict, Verdict::Hard);
        assert_eq!(cut.fault, Some(Fault::Transient("mid-stream")));
        assert!(cut.done);
    }
}

fn finish_reply(ingress: &'static str, frames: &[Vec<u8>]) -> (Piece<'static>, Vec<u8>) {
    let context = Context::new(ingress);
    let ctx = context.borrow();
    let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"text/event-stream")]);
    let mut output = Vec::new();
    for bytes in frames {
        let piece = reply.feed(&Host, &ctx, bytes, false, At::default());
        output.extend_from_slice(&piece.bytes);
    }

    let piece = reply.feed(&Host, &ctx, b"", true, At::default());
    output.extend_from_slice(&piece.bytes);
    (piece, output)
}

#[test]
fn local_encoding_refusal_retains_reported_usage_without_a_success_event() {
    let frames = vec![
        frame("message_start", &start(None, json!({"input_tokens":5000}))),
        frame(
            "content_block_start",
            &json!({"index":0,"content_block":{"type":"tool_use","id":"tool-native","name":"send"}}),
        ),
        frame(
            "content_block_delta",
            &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"not json"}}),
        ),
        frame("content_block_stop", &json!({"index":0})),
        frame("message_delta", &json!({"usage":{"output_tokens":812}})),
    ];
    let (piece, output) = finish_reply("gemini", &frames);
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (5000, 812));
    assert_eq!(
        piece.fault,
        Some(Fault::Transient("stream-translate-abort"))
    );
    assert_eq!(piece.verdict, Verdict::Hard);
    assert!(piece.done);
    let wire = String::from_utf8_lossy(&output);
    assert!(
        wire.contains("INTERNAL"),
        "the actual refusal must reach the client: {wire}"
    );
    assert!(!wire.contains("functionCall"), "{wire}");
    assert!(!wire.contains("finishReason"), "{wire}");
}

#[test]
fn native_upstream_error_keeps_its_existing_refund_disposition() {
    let mut frames = content_frames();
    frames[0] = frame(
        "message_start",
        &start(Some(json!("assistant")), json!({"input_tokens":5000})),
    );
    *frames.last_mut().unwrap() = frame(
        "message_delta",
        &json!({"delta":{},"usage":{"output_tokens":812}}),
    );
    for ingress in ["anthropic", "openai", "gemini"] {
        let mut probe = stream(ingress);
        for native in &frames {
            probe.feed(&Host, native);
        }
        assert_eq!(
            counts(&probe),
            Some((5000, 812)),
            "refund control must first establish real reported work"
        );
        let mut failed = frames.clone();
        failed.push(frame(
            "error",
            &json!({"type":"error","error":{"type":"overloaded_error","message":"try later"}}),
        ));
        let (piece, _) = finish_reply(ingress, &failed);
        assert_eq!(
            (piece.units.tokens_in, piece.units.tokens_out),
            (0, 0),
            "{ingress}: {piece:?}"
        );
        assert_eq!(piece.verdict, Verdict::Hard);
        assert_eq!(piece.fault, Some(Fault::Transient("stream-terminal-error")));
        assert!(piece.done);
    }
}

#[test]
fn recovered_usage_stays_observable_but_native_error_refunds_the_final_reply() {
    for ingress in ["anthropic", "openai", "gemini"] {
        let mut frames = content_frames();
        frames.push(frame(
            "error",
            &json!({"type":"error","error":{"type":"overloaded_error","message":"try later"}}),
        ));
        let mut translated = stream(ingress);
        for native in &frames {
            translated.feed(&Host, native);
        }
        assert_eq!(counts(&translated), Some((5000, 812)));
        assert!(translated.terminal_error().is_some());
        let (piece, _) = finish_reply(ingress, &frames);
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (0, 0));
        assert!(piece.units.open.is_empty());
        assert_eq!(piece.fault, Some(Fault::Transient("stream-terminal-error")));
        assert_eq!(piece.verdict, Verdict::Hard);
    }
}
