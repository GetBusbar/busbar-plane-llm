//! Whole provider reports are checked before bounded delivery and before an oversized body completes.

use crate::codec::cells::CodecHost;
use crate::exchange::arrive::Arrived;
use crate::exchange::attempt::StreamIntent;
use crate::exchange::reply::{At, Reply, ReplyCtx};
use crate::exchange::shaping::Lane;
use busbar_contract::operation::OpVerb;
use serde_json::json;

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

fn with_reply(cap: usize, check: impl FnOnce(&mut Reply, &ReplyCtx<'_>)) {
    let request = json!({"model":"model", "stream":false});
    let arrived = Arrived {
        dialect: "openai",
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
        dialect: "openai",
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
        intent: StreamIntent::default(),
        passthrough: false,
        translate_cap: cap,
    };
    let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"application/json")]);
    check(&mut reply, &ctx);
}

fn response(raw: &str, padding: usize) -> Vec<u8> {
    format!(
        r#"{{"id":"count","object":"chat.completion","model":"model","choices":[{{"index":0,"message":{{"role":"assistant","content":"{}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{raw},"completion_tokens":0,"total_tokens":{raw}}}}}"#,
        "x".repeat(padding)
    ).into_bytes()
}

#[test]
fn fragmented_bounded_refusal_precedes_the_first_header_and_body() {
    let body = response("27.0000001", 0);
    for chunk in [1, 7, body.len()] {
        with_reply(4096, |reply, ctx| {
            let head = reply.feed(&Host, ctx, &[], false, At::default());
            assert!(head.head.is_none() && head.bytes.is_empty());
            let mut refused = false;
            for bytes in body.chunks(chunk) {
                let piece = reply.feed(&Host, ctx, bytes, false, At::default());
                assert!(piece.head.is_none() && piece.bytes.is_empty());
                refused |= !piece.units.refused_counts.is_empty();
            }
            assert!(refused, "chunk {chunk}: refusal must precede transport EOF");
        });
    }
}

#[test]
fn oversized_valid_response_keeps_every_original_byte() {
    let body = response("27.5", 512);
    for chunk in [1, 7, body.len()] {
        with_reply(128, |reply, ctx| {
            let mut received = Vec::new();
            let mut headers = 0;
            for bytes in body.chunks(chunk) {
                let piece = reply.feed(&Host, ctx, bytes, false, At::default());
                headers += usize::from(piece.head.is_some());
                assert!(piece.units.refused_counts.is_empty());
                received.extend_from_slice(&piece.bytes);
            }
            let end = reply.feed(&Host, ctx, &[], true, At::default());
            headers += usize::from(end.head.is_some());
            received.extend_from_slice(&end.bytes);
            assert_eq!(received, body);
            assert_eq!(headers, 1);
            assert!(end.done);
            assert!(end.units.refused_counts.is_empty());
            assert_eq!(
                end.units.exact_counts.as_ref().unwrap()["input"],
                busbar_contract::Count::parse("27.5").unwrap()
            );
        });
    }
}

#[test]
fn oversized_late_invalid_report_refuses_before_the_body_can_complete() {
    let body = response("27.0000001", 512);
    for chunk in [1, 7, body.len()] {
        with_reply(128, |reply, ctx| {
            let mut released = 0;
            let mut refused = false;
            for bytes in body.chunks(chunk) {
                let piece = reply.feed(&Host, ctx, bytes, false, At::default());
                if !piece.units.refused_counts.is_empty() {
                    // The live door sends COUNT_REFUSED with this piece; the kernel
                    // cancels before copying its bytes or checkpointing its units.
                    assert!(released < body.len());
                    assert!(!piece
                        .units
                        .exact_counts
                        .as_ref()
                        .unwrap()
                        .contains_key("input"));
                    assert!(!piece.done, "the transport has not reached EOF");
                    refused = true;
                    break;
                }
                released += piece.bytes.len();
            }
            assert!(
                refused,
                "chunk {chunk}: invalid usage must not wait until EOF"
            );
        });
    }
}

#[test]
fn exact_numeric_ingress_whole_reply_preserves_microcounts_and_bytes() {
    for raw in ["9007199254740.000001", "9223372036854.775807"] {
        let body = response(raw, 512);
        for cap in [128, 4096] {
            with_reply(cap, |reply, ctx| {
                let mut received = Vec::new();
                for bytes in body.chunks(7) {
                    let piece = reply.feed(&Host, ctx, bytes, false, At::default());
                    assert!(piece.units.refused_counts.is_empty());
                    received.extend_from_slice(&piece.bytes);
                }
                let end = reply.feed(&Host, ctx, &[], true, At::default());
                received.extend_from_slice(&end.bytes);
                assert_eq!(received, body);
                assert!(end.done);
                assert!(!end.units.floor);
                assert!(end.units.refused_counts.is_empty());
                assert_eq!(
                    end.units.exact_counts.as_ref().unwrap()["input"],
                    busbar_contract::Count::parse(raw).unwrap()
                );
            });
        }
    }
}

#[test]
fn exact_numeric_ingress_whole_reply_refuses_long_decimal_before_eof() {
    for raw in ["27.0000000000000001", "9223372036854.775808"] {
        let body = response(raw, 512);
        for cap in [128, 4096] {
            with_reply(cap, |reply, ctx| {
                let mut refused = false;
                for bytes in body.chunks(7) {
                    let piece = reply.feed(&Host, ctx, bytes, false, At::default());
                    if !piece.units.refused_counts.is_empty() {
                        assert!(!piece.units.floor);
                        assert!(!piece
                            .units
                            .exact_counts
                            .as_ref()
                            .unwrap()
                            .contains_key("input"));
                        assert!(!piece.done);
                        refused = true;
                        break;
                    }
                    if cap == 4096 {
                        assert!(piece.head.is_none() && piece.bytes.is_empty());
                    }
                }
                assert!(refused, "{raw}, cap={cap}");
            });
        }
    }
}
