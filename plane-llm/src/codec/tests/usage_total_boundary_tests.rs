// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use crate::codec::ir::{IrStreamEvent, IrUsage, StreamDecodeState};
use crate::codec::proto_codec::protocol_for;
use busbar_contract::billing::TokenUsage;
use serde_json::{json, Value};

fn read_paths(protocol: &str, input: u64, output: u64, total: u64) -> (Vec<IrUsage>, TokenUsage) {
    let codec = protocol_for(protocol).expect("the fixture dialect exists");
    let reader = codec.reader();
    let (body, event, chunk) = if protocol == "openai" {
        let usage =
            json!({"prompt_tokens": input, "completion_tokens": output, "total_tokens": total});
        (
            json!({
                "id": "chatcmpl-boundary", "object": "chat.completion", "created": 1,
                "model": "gpt-4o", "usage": usage,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
            }),
            "",
            json!({"id": "chatcmpl-boundary", "object": "chat.completion.chunk", "created": 1,
                "model": "gpt-4o", "choices": [], "usage": usage}),
        )
    } else {
        let usage = json!({"input_tokens": input, "output_tokens": output, "total_tokens": total});
        (
            json!({
                "id": "resp_boundary", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o", "usage": usage,
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}]
            }),
            "response.completed",
            json!({"response": {"status": "completed", "usage": usage}}),
        )
    };
    let buffered = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read the complete response")
        .usage;
    let streamed = reader
        .read_response_events(
            crate::codec::test_host::HOST,
            event,
            &chunk,
            &mut StreamDecodeState::default(),
        )
        .into_iter()
        .find_map(|event| match event {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("the terminal wire usage reaches the stream billing tap");
    let recovered = reader
        .recover_truncated_usage(body.to_string().as_bytes())
        .expect("the tail reader recovers the stated usage");
    (vec![buffered, streamed], recovered)
}

fn assert_billed(usage: &TokenUsage, input: u64, output: u64, gap: u64) {
    assert_eq!((usage.input, usage.output), (input, output));
    let expected: std::collections::BTreeMap<_, _> = if gap == 0 {
        Default::default()
    } else {
        [("unitemized_tokens".to_owned(), gap)].into()
    };
    assert_eq!(
        usage.open_units, expected,
        "the exact positive gap is billed once"
    );
    assert!(usage.residual_units.is_empty());
}

#[test]
fn stated_totals_preserve_exact_gaps_through_readers_and_billing() {
    let signed_max = (i64::MAX / 1_000_000) as u64;
    for (input, output, total, diagnostic, gap) in [
        (signed_max - 1, 0, signed_max, Some(1), 1),
        (signed_max, 0, signed_max - 1, Some(-1), 0),
        (0, 0, signed_max, Some(signed_max as i64), signed_max),
        (signed_max, 0, 0, Some(-(signed_max as i64)), 0),
        (signed_max, 0, signed_max, None, 0),
        (11, 7, 19, Some(1), 1),
        (11, 7, 17, Some(-1), 0),
        (11, 7, 18, None, 0),
    ] {
        for protocol in ["openai", "responses"] {
            let (reads, recovered) = read_paths(protocol, input, output, total);
            for usage in reads {
                assert_eq!(usage.billable_tokens(), input.saturating_add(output));
                assert_eq!(
                    usage.detail.usage_identity_note.as_ref().map(|n| (
                        n.reported_total,
                        n.summed_total,
                        n.unaccounted
                    )),
                    diagnostic.map(|n| (total, input.saturating_add(output), n)),
                    "{protocol}: ({input}, {output}, {total})"
                );
                assert_billed(&usage.to_token_usage(), input, output, gap);
            }
            assert_billed(&recovered, input, output, gap);
        }
    }
}

#[test]
fn absent_unreadable_or_refused_totals_do_not_invent_a_billed_gap() {
    let codec = protocol_for("openai").expect("OpenAI exists");
    for usage in [
        json!({"prompt_tokens": 11, "completion_tokens": 7}),
        json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": "19"}),
        json!({"prompt_tokens": "11", "completion_tokens": 7, "total_tokens": 19}),
    ] {
        let input = if usage["prompt_tokens"].is_string() {
            0
        } else {
            11
        };
        let body: Value = json!({
            "id": "chatcmpl-control", "object": "chat.completion", "created": 1,
            "model": "gpt-4o", "usage": usage,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
        });
        let ir = codec
            .reader()
            .read_response(crate::codec::test_host::HOST, &body)
            .expect("read the control");
        assert!(ir.usage.detail.usage_identity_note.is_none());
        assert_billed(&ir.usage.to_token_usage(), input, 7, 0);
    }
}

#[test]
fn out_of_store_range_whole_counts_are_refused_in_every_live_reader() {
    for input in [(i64::MAX / 1_000_000) as u64 + 1, i64::MAX as u64, u64::MAX] {
        for protocol in ["openai", "responses"] {
            let (reads, recovered) = read_paths(protocol, input, 7, input);
            for usage in reads
                .into_iter()
                .map(|ir| ir.to_token_usage())
                .chain([recovered])
            {
                assert!(!usage.refused_counts.is_empty(), "{protocol}: {input}");
                assert!(!usage.exact_counts.as_ref().unwrap().contains_key("input"));
                assert_eq!(
                    usage.exact_counts.as_ref().unwrap()["output"],
                    busbar_contract::Count::from_integer(7).unwrap()
                );
            }
        }
    }
}
