//! MONEY BUG regression: `serde_json::Value::as_u64()` returns `None` for ANY float-backed

use super::super::proto_codec::ProtocolReader;
use super::AnthropicReader;
use crate::codec::ir::IrStreamEvent;

/// stream:`message_start.message.usage` — `input_tokens`/`output_tokens` float-encoded must survive

#[test]
fn message_start_float_token_counts_survive_as_integers() {
    let data = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_1", "role": "assistant", "model": "claude-x",
            "usage": {
                "input_tokens": 27.0, "output_tokens": 9.0,
                "cache_creation_input_tokens": 4.0, "cache_read_input_tokens": 3.0
            }
        }
    });
    let ev = AnthropicReader
        .read_response_event(crate::codec::test_host::HOST, "message_start", &data)
        .expect("message_start parses");
    let IrStreamEvent::MessageStart { usage, .. } = ev else {
        panic!("expected MessageStart");
    };
    let usage = usage.expect("usage present");
    assert_eq!(
        usage.input_tokens, 27,
        "float-encoded input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output_tokens, 9,
        "float-encoded output_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_creation_input_tokens,
        Some(4),
        "float-encoded cache_creation_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_read_input_tokens,
        Some(3),
        "float-encoded cache_read_input_tokens must survive as the exact integer, not 0"
    );
}

/// stream:`message_start.message.usage.cache_creation` — the 5m/1h TTL tier split, float-encoded,

#[test]
fn message_start_float_cache_tier_split_survives_as_integers() {
    let data = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_1", "role": "assistant", "model": "claude-x",
            "usage": {
                "input_tokens": 1, "output_tokens": 1,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 4.0, "ephemeral_1h_input_tokens": 3.0
                }
            }
        }
    });
    let ev = AnthropicReader
        .read_response_event(crate::codec::test_host::HOST, "message_start", &data)
        .expect("message_start parses");
    let IrStreamEvent::MessageStart { usage, .. } = ev else {
        panic!("expected MessageStart");
    };
    let usage = usage.expect("usage present");
    assert_eq!(
        usage.detail.cache_creation_5m_input_tokens,
        Some(4),
        "float-encoded ephemeral_5m_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.detail.cache_creation_1h_input_tokens,
        Some(3),
        "float-encoded ephemeral_1h_input_tokens must survive as the exact integer, not 0"
    );
}

/// stream:`message_delta.usage` — the terminal streaming usage frame. Float-encoded

#[test]
fn message_delta_float_token_counts_survive_as_integers() {
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {
            "input_tokens": 5.0, "output_tokens": 27.0,
            "cache_creation_input_tokens": 8.0, "cache_read_input_tokens": 6.0
        }
    });
    let ev = AnthropicReader
        .read_response_event(crate::codec::test_host::HOST, "message_delta", &data)
        .expect("message_delta parses");
    let IrStreamEvent::MessageDelta { usage, .. } = ev else {
        panic!("expected MessageDelta");
    };
    assert_eq!(
        usage.input_tokens, 5,
        "float-encoded input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output_tokens, 27,
        "float-encoded output_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_creation_input_tokens,
        Some(8),
        "float-encoded cache_creation_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_read_input_tokens,
        Some(6),
        "float-encoded cache_read_input_tokens must survive as the exact integer, not 0"
    );
}

/// Buffered `read_response().usage` — every top-level counter, the 5m/1h cache tier split,

#[test]
fn buffered_response_float_usage_counts_survive_as_integers() {
    let body = serde_json::json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-3-5-sonnet",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10.0, "output_tokens": 27.0,
            "cache_creation_input_tokens": 7.0, "cache_read_input_tokens": 3.0,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 4.0, "ephemeral_1h_input_tokens": 3.0
            },
            "server_tool_use": {"web_search_requests": 2.0},
            "output_tokens_details": {"thinking_tokens": 6.0}
        }
    });
    let ir = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("parses");
    assert_eq!(
        ir.usage.input_tokens, 10,
        "float-encoded input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.output_tokens, 27,
        "float-encoded output_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.cache_creation_input_tokens,
        Some(7),
        "float-encoded cache_creation_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.cache_read_input_tokens,
        Some(3),
        "float-encoded cache_read_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.detail.cache_creation_5m_input_tokens,
        Some(4),
        "float-encoded ephemeral_5m_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.detail.cache_creation_1h_input_tokens,
        Some(3),
        "float-encoded ephemeral_1h_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.detail.web_search_requests,
        Some(2),
        "float-encoded web_search_requests must survive as the exact integer, not 0"
    );
    assert_eq!(
        ir.usage.detail.reasoning_tokens,
        Some(6),
        "float-encoded thinking_tokens must survive as the exact integer, not 0"
    );
}

/// MONEY BUG regression (truncated-tail recovery path): a HEAD-truncated non-stream billing buffer

#[test]
fn recover_truncated_usage_float_token_counts_survive_as_integers() {
    let tail = br#"{"usage":{"input_tokens":27.0,"output_tokens":9.0,"cache_creation_input_tokens":4.0,"cache_read_input_tokens":3.0}}"#;
    let usage = AnthropicReader
        .recover_truncated_usage(tail)
        .expect("usage tail must be recoverable");
    assert_eq!(
        usage.input, 27,
        "float-encoded input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.output, 9,
        "float-encoded output_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_creation,
        Some(4),
        "float-encoded cache_creation_input_tokens must survive as the exact integer, not 0"
    );
    assert_eq!(
        usage.cache_read,
        Some(3),
        "float-encoded cache_read_input_tokens must survive as the exact integer, not 0"
    );
}
