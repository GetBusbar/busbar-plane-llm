use crate::codec::bedrock::bedrock_response_to_eventstream;
use crate::codec::ir::{IrBlock, IrResponse, IrRole, IrUsage};

/// A bedrock-ingress ConverseStream request answered by a BUFFERED (non-SSE) 2xx is rewrapped
/// into the native binary eventstream frame sequence — not an `application/json` Converse body
/// the AWS SDK's stream decoder cannot parse.
#[test]
fn buffered_response_wraps_into_converse_stream_frames() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![IrBlock::text("hello world".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(42));
    assert!(!bytes.is_empty(), "must emit eventstream frames");

    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let names: Vec<&str> = frames.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names.first(), Some(&"messageStart"));

    assert!(
        !names.contains(&"contentBlockStart"),
        "a text-only response must emit no contentBlockStart frame; got {names:?}"
    );
    assert!(names.contains(&"contentBlockDelta"));
    assert!(names.contains(&"contentBlockStop"));
    assert!(names.contains(&"messageStop"));

    let metadata = frames
        .iter()
        .find(|(t, _)| t == "metadata")
        .expect("metadata frame");
    let payload: serde_json::Value = serde_json::from_slice(&metadata.1).expect("json");
    assert_eq!(payload["usage"]["inputTokens"], 11);
    assert_eq!(payload["usage"]["outputTokens"], 7);
    assert_eq!(payload["usage"]["totalTokens"], 18);

    assert_eq!(
        payload["metrics"]["latencyMs"].as_u64(),
        Some(42),
        "buffered metadata frame must carry metrics.latencyMs like the live path: {payload}"
    );
}

/// The `IrBlock::ToolUse` arm of `bedrock_response_to_eventstream`
#[test]
fn buffered_tool_use_wraps_into_converse_stream_tool_frames() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![IrBlock::ToolUse {
            thought_signature: None,
            id: "toolu_abc123".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "Paris"}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: IrUsage {
            input_tokens: 5,
            output_tokens: 9,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(7));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);

    let start = frames
        .iter()
        .find(|(t, _)| t == "contentBlockStart")
        .expect("contentBlockStart frame");
    let start_payload: serde_json::Value =
        serde_json::from_slice(&start.1).expect("json contentBlockStart");
    assert_eq!(
        start_payload["start"]["toolUse"]["toolUseId"], "toolu_abc123",
        "tool start carries toolUseId: {start_payload}"
    );
    assert_eq!(
        start_payload["start"]["toolUse"]["name"], "get_weather",
        "tool start carries name: {start_payload}"
    );

    let delta = frames
        .iter()
        .find(|(t, _)| t == "contentBlockDelta")
        .expect("contentBlockDelta frame");
    let delta_payload: serde_json::Value =
        serde_json::from_slice(&delta.1).expect("json contentBlockDelta");
    let input_str = delta_payload["delta"]["toolUse"]["input"]
        .as_str()
        .expect("tool input is a serialized JSON string");
    let input: serde_json::Value = serde_json::from_str(input_str).expect("input decodes to JSON");
    assert_eq!(
        input["city"], "Paris",
        "tool input round-trips through the delta: {delta_payload}"
    );
}

/// A MULTI-block buffered cross-protocol completion (the
/// canonical 'assistant says something then calls a tool') must emit a DISTINCT, monotonically
/// increasing `contentBlockIndex` per block — 0 for block 0, 1 for block 1, 2 for block 2.
#[test]
fn buffered_multi_block_assigns_distinct_monotonic_content_block_indices() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![
            IrBlock::text("Let me check the weather.".to_string()),
            IrBlock::ToolUse {
                thought_signature: None,
                id: "toolu_abc123".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "Paris"}),
                cache_control: None,
            },
            IrBlock::text("One moment.".to_string()),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        usage: IrUsage {
            input_tokens: 12,
            output_tokens: 20,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(99));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let names: Vec<&str> = frames.iter().map(|(t, _)| t.as_str()).collect();

    assert_eq!(names.first(), Some(&"messageStart"), "frames: {names:?}");
    let block_frames: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.starts_with("contentBlock"))
        .collect();
    assert_eq!(
        block_frames,
        vec![
            // block 0: Text (no start)
            "contentBlockDelta",
            "contentBlockStop",
            // block 1: ToolUse (start + delta + stop)
            "contentBlockStart",
            "contentBlockDelta",
            "contentBlockStop",
            // block 2: Text (no start)
            "contentBlockDelta",
            "contentBlockStop",
        ],
        "Text blocks emit delta/stop; the ToolUse block emits start/delta/stop, in order: {names:?}"
    );
    let stop_pos = names
        .iter()
        .position(|n| *n == "messageStop")
        .expect("messageStop");
    let meta_pos = names
        .iter()
        .position(|n| *n == "metadata")
        .expect("metadata");
    assert!(
        stop_pos < meta_pos,
        "messageStop precedes metadata: {names:?}"
    );
    let last_block_pos = names
        .iter()
        .rposition(|n| n.starts_with("contentBlock"))
        .expect("a contentBlock frame");
    assert!(
        last_block_pos < stop_pos,
        "all content blocks precede messageStop: {names:?}"
    );

    let block_indices: Vec<u64> = frames
        .iter()
        .filter(|(t, _)| t.starts_with("contentBlock"))
        .map(|(t, body)| {
            let v: serde_json::Value = serde_json::from_slice(body).expect("content frame JSON");
            v["contentBlockIndex"]
                .as_u64()
                .unwrap_or_else(|| panic!("{t} frame must carry contentBlockIndex: {v}"))
        })
        .collect();
    assert_eq!(
        block_indices,
        vec![0, 0, 1, 1, 1, 2, 2],
        "each block's frames carry its own distinct, monotonic index; never colliding"
    );
}

/// A buffered cross-protocol completion whose IR carried a
/// ToolUse block but NO explicit `stop_reason` must synthesize `messageStop.stopReason ==
/// "tool_use"`, not the old unconditional `end_turn`.
#[test]
fn buffered_tool_use_with_absent_stop_reason_defaults_to_tool_use() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![IrBlock::ToolUse {
            thought_signature: None,
            id: "toolu_xyz".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "Paris"}),
            cache_control: None,
        }],

        stop_reason: None,
        usage: IrUsage {
            input_tokens: 5,
            output_tokens: 9,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(3));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let stop = frames
        .iter()
        .find(|(t, _)| t == "messageStop")
        .expect("messageStop frame");
    let payload: serde_json::Value = serde_json::from_slice(&stop.1).expect("json messageStop");
    assert_eq!(
        payload["stopReason"], "tool_use",
        "absent stop_reason with a ToolUse block must default to tool_use, not end_turn: {payload}"
    );
}

/// The companion to the test above — a TEXT-only completion
/// with an absent `stop_reason` must STILL default to `end_turn` (no tool call → no tool_use).
#[test]
fn buffered_text_only_with_absent_stop_reason_defaults_to_end_turn() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![IrBlock::text("All done.".to_string())],
        stop_reason: None,
        usage: IrUsage {
            input_tokens: 3,
            output_tokens: 4,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(3));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let stop = frames
        .iter()
        .find(|(t, _)| t == "messageStop")
        .expect("messageStop frame");
    let payload: serde_json::Value = serde_json::from_slice(&stop.1).expect("json messageStop");
    assert_eq!(
        payload["stopReason"], "end_turn",
        "absent stop_reason with no tool call must remain end_turn: {payload}"
    );
}

/// An EXPLICIT `stop_reason` always wins over the
/// content-derived default — a ToolUse block alongside an explicit `end_turn` keeps `end_turn`.
#[test]
fn buffered_explicit_stop_reason_overrides_content_default() {
    let ir = IrResponse {
        logprobs: Vec::new(),
        role: IrRole::Assistant,
        content: vec![IrBlock::ToolUse {
            thought_signature: None,
            id: "toolu_xyz".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "Paris"}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: IrUsage {
            input_tokens: 5,
            output_tokens: 9,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("anthropic.claude-3".to_string()),
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let mut bytes = bedrock_response_to_eventstream(crate::codec::test_host::HOST, &ir, Some(3));
    let frames = crate::codec::eventstream::drain_frames(&mut bytes);
    let stop = frames
        .iter()
        .find(|(t, _)| t == "messageStop")
        .expect("messageStop frame");
    let payload: serde_json::Value = serde_json::from_slice(&stop.1).expect("json messageStop");
    assert_eq!(
        payload["stopReason"], "end_turn",
        "explicit stop_reason must override the content-derived default: {payload}"
    );
}
