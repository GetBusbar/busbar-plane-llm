// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! MONEY BUG regression: `serde_json::Value::as_u64()` returns `None` for ANY float-backed

use super::*;

/// stream:`metadata.usage` — the ConverseStream terminal usage frame. Float-encoded
#[test]
fn stream_metadata_float_token_counts_survive_as_integers() {
    let reader = BedrockReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type": "messageStop", "stopReason": "end_turn"}),
        &mut state,
    );

    let ev = reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({
            "type": "metadata",
            "usage": {
                "inputTokens": 5.0, "outputTokens": 27.0,
                "cacheWriteInputTokens": 8.0, "cacheReadInputTokens": 6.0
            }
        }),
        &mut state,
    );
    let Some(crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. }) = ev.first() else {
        panic!("expected a MessageDelta carrying usage; got {ev:?}");
    };
    assert_eq!(
        usage.input_tokens, 5,
        "float-encoded inputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output_tokens, 27,
        "float-encoded outputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_creation_input_tokens,
        Some(8),
        "float-encoded cacheWriteInputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_read_input_tokens,
        Some(6),
        "float-encoded cacheReadInputTokens must survive as the exact integer, not 0"
    );
}

/// Buffered `read_response().usage` — `inputTokens`/`outputTokens`/`cacheWriteInputTokens`/
#[test]
fn buffered_response_float_usage_counts_survive_as_integers() {
    let reader = BedrockReader;
    let body = serde_json::json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 10.0, "outputTokens": 27.0,
            "cacheReadInputTokens": 3.0, "cacheWriteInputTokens": 7.0
        }
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read");
    assert_eq!(
        resp.usage.input_tokens, 10,
        "float-encoded inputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        resp.usage.output_tokens, 27,
        "float-encoded outputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        resp.usage.cache_read_input_tokens,
        Some(3),
        "float-encoded cacheReadInputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        resp.usage.cache_creation_input_tokens,
        Some(7),
        "float-encoded cacheWriteInputTokens must survive as the exact integer, not 0"
    );
}

/// MONEY BUG regression (truncated-tail recovery path): a HEAD-truncated non-stream billing buffer
#[test]
fn recover_truncated_usage_float_token_counts_survive_as_integers() {
    let reader = BedrockReader;
    let tail = br#"{"usage":{"inputTokens":27.0,"outputTokens":9.0,"cacheWriteInputTokens":4.0,"cacheReadInputTokens":3.0}}"#;
    let usage = reader
        .recover_truncated_usage(tail)
        .expect("usage tail must be recoverable");
    assert_eq!(
        usage.input, 27,
        "float-encoded inputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output, 9,
        "float-encoded outputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_creation,
        Some(4),
        "float-encoded cacheWriteInputTokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_read,
        Some(3),
        "float-encoded cacheReadInputTokens must survive as the exact integer, not 0"
    );
}
