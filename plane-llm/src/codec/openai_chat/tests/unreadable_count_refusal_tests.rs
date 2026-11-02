//! Unreadable billed counts are refused independently while readable counts remain billable.

use super::*;

#[test]
fn buffered_response_refuses_a_stringified_count_and_bills_the_readable_ones() {
    let body = serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": "1500", "completion_tokens": 9}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("an unreadable count refuses itself, never the answer");
    assert_eq!(
        (ir.usage.input_tokens, ir.usage.output_tokens),
        (0, 9),
        "the refused prompt_tokens bills nothing; completion_tokens bills as stated"
    );
}

#[test]
fn buffered_response_with_an_absent_count_still_reads_as_zero() {
    let body = serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"completion_tokens": 9}
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("an absent count is zero");
    assert_eq!(ir.usage.input_tokens, 0);
    assert_eq!(ir.usage.output_tokens, 9);
}

#[test]
fn streaming_usage_chunk_refuses_a_stringified_count_and_bills_the_readable_ones() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let chunk = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "model": "gpt-4o",
        "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": "9"}
    });
    let ev =
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state);
    assert!(
        !ev.iter().any(|e| matches!(e, IrStreamEvent::Error(_))),
        "an unreadable count refuses itself, never the stream; got {ev:?}"
    );
    let streamed = ev
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage.to_token_usage()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the usage chunk carries its readable usage: {ev:?}"));
    assert_eq!(
        (streamed.input, streamed.output),
        (10, 0),
        "prompt_tokens bills as stated; the refused completion_tokens bills nothing"
    );
}

#[test]
fn truncated_recovery_refuses_a_stringified_count_and_recovers_the_readable_ones() {
    let tail = br#"{"usage":{"prompt_tokens":"1500","completion_tokens":9}}"#;
    let u = OpenAiReader
        .recover_truncated_usage(tail)
        .expect("the readable counts recover");
    assert_eq!((u.input, u.output), (0, 9));
    let tail = br#"{"usage":{"completion_tokens":9}}"#;
    let u = OpenAiReader
        .recover_truncated_usage(tail)
        .expect("an absent count still recovers");
    assert_eq!((u.input, u.output), (0, 9));
}

fn chat_body(usage: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": usage
    })
}

/// The cache-write slice bills inside the input on every path and in every spelling, as in 1.5.5:

#[test]
fn cache_write_tokens_bill_inside_the_input_on_every_path() {
    for spelling in [
        serde_json::json!(40),
        serde_json::json!("40"),
        serde_json::Value::Null,
    ] {
        let usage = serde_json::json!({
            "prompt_tokens": 100, "completion_tokens": 9,
            "prompt_tokens_details": {"cache_write_tokens": spelling}
        });
        let ir = OpenAiReader
            .read_response(crate::codec::test_host::HOST, &chat_body(usage.clone()))
            .expect("the cache-write slice never refuses the answer");
        let t = ir.usage.to_token_usage();
        assert_eq!((t.input, t.output, t.cache_creation), (100, 9, None));

        let mut state = crate::codec::ir::StreamDecodeState::default();
        let chunk = serde_json::json!({
            "id": "c", "object": "chat.completion.chunk", "model": "gpt-4o",
            "choices": [],
            "usage": usage
        });
        let ev = OpenAiReader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut state,
        );
        let streamed = ev
            .iter()
            .find_map(|e| match e {
                IrStreamEvent::MessageDelta { usage, .. } => Some(usage.to_token_usage()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the usage chunk carries its usage: {ev:?}"));
        assert_eq!(
            (streamed.input, streamed.output, streamed.cache_creation),
            (100, 9, None)
        );

        let tail = format!(r#"{{"usage":{usage}}}"#);
        let recovered = OpenAiReader
            .recover_truncated_usage(tail.as_bytes())
            .expect("the usage recovers");
        assert_eq!(
            (recovered.input, recovered.output, recovered.cache_creation),
            (100, 9, None)
        );
    }
}
