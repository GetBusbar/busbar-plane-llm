use super::*;
use crate::codec::ir::{IrBlockMeta, IrDelta, IrRole, IrStreamEvent, IrUsage, StreamDecodeState};
use serde_json::json;

#[test]
fn test_openai_read_fanout_text() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    for chunk in [
        json!({"choices":[{"delta":{"role":"assistant"}}]}),
        json!({"choices":[{"delta":{"content":"Hel"}}]}),
        json!({"choices":[{"delta":{"content":"lo"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
    ] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut st,
        ));
    }
    assert_eq!(
        events,
        vec![
            IrStreamEvent::MessageStart {
                role: IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None
            },
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::Text,
                refusal: false,
            },
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::TextDelta("Hel".to_string())
            },
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::TextDelta("lo".to_string())
            },
            IrStreamEvent::BlockStop { index: 0 },
            IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: IrUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    detail: crate::codec::ir::IrUsageDetail::default(),
                },
                stop_detail: None,
            },
            IrStreamEvent::MessageStop,
        ]
    );
}

#[test]
fn test_openai_read_fanout_tool_call() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    for chunk in [
        json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc\":\"SF\"}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut st,
        ));
    }
    assert_eq!(
        events,
        vec![
            IrStreamEvent::MessageStart {
                role: IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None
            },
            // Tool-only stream (no text) is 0-based — text reserves index 0 ONLY
            // when text actually appears. Previously asserted the buggy 1-based index.
            IrStreamEvent::BlockStart {
                index: 0,
                block: IrBlockMeta::ToolUse {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string()
                },
                refusal: false,
            },
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::InputJsonDelta(String::new())
            },
            IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::InputJsonDelta("{\"loc\":\"SF\"}".to_string())
            },
            IrStreamEvent::BlockStop { index: 0 },
            IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
                stop_sequence: None,
                usage: IrUsage::default(),
                stop_detail: None,
            },
            IrStreamEvent::MessageStop,
        ]
    );
}

/// INV-C: a backend that streams `tool_calls[{index:1}]` with NO index 0 (observed from vLLM /
#[test]
fn openai_sparse_tool_index_does_not_collide_text_onto_tool_block() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    for chunk in [
        json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"content":"hi"}}]}),
    ] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut st,
        ));
    }

    let tool_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { .. },
            refusal: _,
        } => Some(*index),
        _ => None,
    });
    let text_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::Text,
            refusal: _,
        } => Some(*index),
        _ => None,
    });

    let tool_index = tool_index.expect("the tool block must open a BlockStart");
    let text_index = text_index.expect("the text block must open a BlockStart");
    assert_ne!(
        tool_index, text_index,
        "tool and text must not share an IR index: tool={tool_index:?} text={text_index:?}, events={events:?}"
    );

    let mut start_indices: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    let n = start_indices.len();

    assert_eq!(
        n, 2,
        "exactly two BlockStart frames (one tool, one text) are expected; got {events:?}"
    );
    start_indices.sort_unstable();
    start_indices.dedup();
    assert_eq!(
        start_indices.len(),
        n,
        "no two BlockStart frames may share an index; got duplicate in {events:?}"
    );
}

/// INV-C: `tool_calls[{index:1}]`, then a text chunk, then `tool_calls[{index:0}]` (the OTHER
#[test]
fn openai_tool_index_zero_after_text_does_not_collide() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    for chunk in [
        json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"content":"hi"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"get_time","arguments":""}}]}}]}),
    ] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut st,
        ));
    }

    let mut start_indices: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            IrStreamEvent::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    start_indices.sort_unstable();
    assert_eq!(
        start_indices,
        vec![0, 1, 2],
        "three blocks (tool, text, tool) must land on three distinct dense indices; got {start_indices:?}, events={events:?}"
    );
}

/// INV-C, the GAP case: a lone `tool_calls[{index:5}]` (no other blocks) must claim IR index 0,
#[test]
fn openai_sparse_tool_indices_are_dense_from_zero() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let events = reader.read_response_events(crate::codec::test_host::HOST,
        "",
        &json!({"choices":[{"delta":{"tool_calls":[{"index":5,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}),
        &mut st,
    );
    let tool_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { .. },
            refusal: _,
        } => Some(*index),
        _ => None,
    });
    assert_eq!(
        tool_index,
        Some(0),
        "a lone tool block must claim IR index 0 (dense from zero), not the raw upstream index 5; events={events:?}"
    );
}

/// REGRESSION PROOF. The current recomputed-base arithmetic happens to land on 2 here, a slot
#[test]
fn openai_tool_after_finish_chunk_claims_a_fresh_index() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    for chunk in [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"content":"hi"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"get_time","arguments":""}}]}}]}),
    ] {
        events.extend(reader.read_response_events(
            crate::codec::test_host::HOST,
            "",
            &chunk,
            &mut st,
        ));
    }

    let pre_finish_tool_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { name, .. },
            refusal: _,
        } if name == "get_weather" => Some(*index),
        _ => None,
    });
    let text_index = events.iter().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::Text,
            refusal: _,
        } => Some(*index),
        _ => None,
    });

    let post_finish_tool_index = events.iter().rev().find_map(|e| match e {
        IrStreamEvent::BlockStart {
            index,
            block: IrBlockMeta::ToolUse { name, .. },
            refusal: _,
        } if name == "get_time" => Some(*index),
        _ => None,
    });

    let post_finish_tool_index =
        post_finish_tool_index.expect("the post-finish tool_calls chunk must still open a block");
    let text_index = text_index.expect("the text chunk must open a text block");
    let pre_finish_tool_index =
        pre_finish_tool_index.expect("the pre-finish get_weather chunk must open a tool block");
    assert_ne!(
        post_finish_tool_index, text_index,
        "the post-finish tool must NOT collide with the text block's index; got post-finish={post_finish_tool_index:?} text={text_index:?}, events={events:?}"
    );
    assert_ne!(
        post_finish_tool_index, pre_finish_tool_index,
        "the post-finish tool must NOT collide with the pre-finish tool's index; got post-finish={post_finish_tool_index:?} pre-finish={pre_finish_tool_index:?}, events={events:?}"
    );
}

#[test]
fn test_openai_read_fanout_cached_tokens() {
    let reader = OpenAiReader;
    let mut st = StreamDecodeState::default();
    let mut events: Vec<IrStreamEvent> = Vec::new();
    events.extend(reader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"choices":[{"delta":{"content":"hi"}}]}),
        &mut st,
    ));
    events.extend(reader.read_response_events(crate::codec::test_host::HOST,
            "",
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":7}}}),
            &mut st,
        ));
    let usage = events
        .iter()
        .find_map(|e| match e {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("MessageDelta present");
    assert_eq!(
        usage.cache_read_input_tokens,
        Some(7),
        "cached_tokens → cache_read"
    );
    assert_eq!(
        usage.cache_creation_input_tokens, None,
        "OpenAI has no cache-creation split"
    );

    assert_eq!(usage.input_tokens, 93);
    assert_eq!(usage.output_tokens, 50);

    assert_eq!(usage.billable_tokens(), 150);
}

#[test]
fn test_anthropic_read_events_wraps_singular() {
    let reader = AnthropicReader;
    let mut st = StreamDecodeState::default();
    let data =
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}});
    let single =
        reader.read_response_event(crate::codec::test_host::HOST, "content_block_delta", &data);
    let plural = reader.read_response_events(
        crate::codec::test_host::HOST,
        "content_block_delta",
        &data,
        &mut st,
    );
    assert_eq!(
        plural,
        single.into_iter().collect::<Vec<_>>(),
        "Anthropic plural wraps singular 1:1"
    );
    assert_eq!(plural.len(), 1);

    assert_eq!(
        reader.read_response_events(crate::codec::test_host::HOST, "ping", &json!({}), &mut st),
        Vec::<IrStreamEvent>::new()
    );
}
