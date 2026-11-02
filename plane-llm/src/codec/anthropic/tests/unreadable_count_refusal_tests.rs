//! Unreadable billed counts are refused independently while readable counts remain billable.

use super::*;

#[test]
fn buffered_response_refuses_a_stringified_count_and_bills_the_readable_ones() {
    let body = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": "1500", "output_tokens": 9}
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("an unreadable count refuses itself, never the answer");
    assert_eq!((ir.usage.input_tokens, ir.usage.output_tokens), (0, 9));
}

#[test]
fn buffered_response_with_an_absent_count_still_reads_as_zero() {
    let body = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"output_tokens": 9}
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("an absent count is zero");
    assert_eq!(ir.usage.input_tokens, 0);
    assert_eq!(ir.usage.output_tokens, 9);
}

/// The (input, output) a streamed frame's event carries, `None` for an error or a frame with none.
fn streamed_tokens(event: &str, data: &serde_json::Value) -> Option<(u64, u64)> {
    match AnthropicReader.read_response_event(crate::codec::test_host::HOST, event, data) {
        Some(IrStreamEvent::MessageStart { usage: Some(u), .. }) => {
            Some((u.input_tokens, u.output_tokens))
        }
        Some(IrStreamEvent::MessageDelta { usage, .. }) => {
            Some((usage.input_tokens, usage.output_tokens))
        }
        _ => None,
    }
}

#[test]
fn streaming_usage_frames_refuse_a_stringified_count_and_bill_the_readable_ones() {
    let start = serde_json::json!({
        "type": "message_start",
        "message": {"id": "m", "role": "assistant", "usage": {"input_tokens": "1500", "output_tokens": 1}}
    });
    assert_eq!(
        streamed_tokens("message_start", &start),
        Some((0, 1)),
        "message_start: the refused input_tokens bills nothing, output_tokens as stated"
    );
    let delta = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"output_tokens": "9"}
    });
    assert_eq!(
        streamed_tokens("message_delta", &delta),
        Some((0, 0)),
        "message_delta: the refused output_tokens bills nothing, and never ends the stream"
    );
}

#[test]
fn truncated_recovery_refuses_a_stringified_count_and_recovers_the_readable_ones() {
    let tail = br#"{"usage":{"input_tokens":"1500","output_tokens":9}}"#;
    let u = AnthropicReader
        .recover_truncated_usage(tail)
        .expect("the readable counts recover");
    assert_eq!((u.input, u.output), (0, 9));
    let tail = br#"{"usage":{"output_tokens":9}}"#;
    let u = AnthropicReader
        .recover_truncated_usage(tail)
        .expect("an absent count still recovers");
    assert_eq!((u.input, u.output), (0, 9));
}

fn message(usage: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": usage
    })
}

fn unreadable_detail_usages() -> [serde_json::Value; 3] {
    [
        serde_json::json!({"input_tokens": 5, "output_tokens": 9, "cache_creation_input_tokens": 40,
                           "cache_creation": {"ephemeral_5m_input_tokens": "40", "ephemeral_1h_input_tokens": 0}}),
        serde_json::json!({"input_tokens": 5, "output_tokens": 9, "cache_creation_input_tokens": 40,
                           "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": "40"}}),
        serde_json::json!({"input_tokens": 5, "output_tokens": 9,
                           "server_tool_use": {"web_search_requests": "2"}}),
    ]
}

#[test]
fn buffered_response_refuses_an_unreadable_cache_tier_or_metered_count_on_its_own() {
    for usage in unreadable_detail_usages() {
        let ir = AnthropicReader
            .read_response(crate::codec::test_host::HOST, &message(usage.clone()))
            .unwrap_or_else(|e| {
                panic!("{usage}: a refused detail count refuses only itself: {e:?}")
            });
        assert_eq!(
            (ir.usage.input_tokens, ir.usage.output_tokens),
            (5, 9),
            "{usage}: the readable totals bill as stated"
        );
        let d = &ir.usage.detail;
        for (value, count) in [
            (
                usage.pointer("/cache_creation/ephemeral_5m_input_tokens"),
                d.cache_creation_5m_input_tokens,
            ),
            (
                usage.pointer("/cache_creation/ephemeral_1h_input_tokens"),
                d.cache_creation_1h_input_tokens,
            ),
            (
                usage.pointer("/server_tool_use/web_search_requests"),
                d.web_search_requests,
            ),
        ] {
            if value.is_some_and(serde_json::Value::is_string) {
                assert_eq!(
                    count, None,
                    "{usage}: refused detail is absent, never coerced"
                );
            }
        }
    }
}

#[test]
fn unreadable_cache_totals_refuse_only_their_own_counts_on_every_read_path() {
    for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
        let mut usage = serde_json::json!({"input_tokens":5,"output_tokens":9,
            "cache_read_input_tokens":11,"cache_creation_input_tokens":13});
        usage[field] = serde_json::json!("1500");
        let expected = if field == "cache_read_input_tokens" {
            (None, Some(13))
        } else {
            (Some(11), None)
        };
        let response = AnthropicReader
            .read_response(crate::codec::test_host::HOST, &message(usage.clone()))
            .unwrap();
        let start = serde_json::json!({"type":"message_start",
            "message":{"role":"assistant","usage":usage.clone()}});
        let delta = serde_json::json!({"type":"message_delta","delta":{},"usage":usage.clone()});
        let streamed = [("message_start", start), ("message_delta", delta)].map(|(event, data)| {
            match AnthropicReader.read_response_event(crate::codec::test_host::HOST, event, &data) {
                Some(IrStreamEvent::MessageStart {
                    usage: Some(usage), ..
                })
                | Some(IrStreamEvent::MessageDelta { usage, .. }) => usage,
                other => panic!("{event}: expected usage, got {other:?}"),
            }
        });
        for usage in [response.usage, streamed[0].clone(), streamed[1].clone()] {
            assert_eq!((usage.input_tokens, usage.output_tokens), (5, 9));
            assert_eq!(
                (
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens
                ),
                expected
            );
        }
        let tail = format!("{{\"usage\":{usage}}}");
        let recovered = AnthropicReader
            .recover_truncated_usage(tail.as_bytes())
            .unwrap();
        assert_eq!((recovered.input, recovered.output), (5, 9));
        assert_eq!((recovered.cache_read, recovered.cache_creation), expected);
    }
}

#[test]
fn buffered_response_tier_counts_absent_null_and_readable_still_read() {
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &message(serde_json::json!({
            "input_tokens": 5, "output_tokens": 9, "cache_creation_input_tokens": 40,
            "cache_creation": {"ephemeral_5m_input_tokens": 40, "ephemeral_1h_input_tokens": null},
            "server_tool_use": {"web_search_requests": 2}
        })))
        .expect("absent, null and readable detail counts read");
    assert_eq!(ir.usage.detail.cache_creation_5m_input_tokens, Some(40));
    assert_eq!(ir.usage.detail.cache_creation_1h_input_tokens, None);
    assert_eq!(ir.usage.detail.web_search_requests, Some(2));
}

#[test]
fn streaming_usage_frames_refuse_an_unreadable_cache_tier_or_metered_count_on_its_own() {
    for usage in unreadable_detail_usages() {
        let start = serde_json::json!({
            "type": "message_start",
            "message": {"id": "m", "role": "assistant", "usage": usage.clone()}
        });
        assert_eq!(
            streamed_tokens("message_start", &start),
            Some((5, 9)),
            "message_start with {usage}: the readable totals bill, the stream does not end"
        );
        let delta = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": usage.clone()
        });
        assert_eq!(
            streamed_tokens("message_delta", &delta),
            Some((5, 9)),
            "message_delta with {usage}: the readable totals bill, the stream does not end"
        );
    }
}

#[test]
fn truncated_recovery_preserves_readable_counts_beside_unreadable_details() {
    for usage in unreadable_detail_usages() {
        let tail = format!("{{\"usage\":{usage}}}");
        let recovered = AnthropicReader
            .recover_truncated_usage(tail.as_bytes())
            .expect("readable counts survive the refused detail");
        assert_eq!((recovered.input, recovered.output), (5, 9));
        assert!(!recovered.open_units.contains_key("search_units"));
        assert_eq!(
            recovered.cache_creation,
            usage["cache_creation_input_tokens"].as_u64()
        );
    }
}
