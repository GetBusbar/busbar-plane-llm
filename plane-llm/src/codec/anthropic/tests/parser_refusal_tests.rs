use super::*;
use crate::codec::test_host::HOST;
use serde_json::{json, Value};

fn request() -> Value {
    json!({"model":"m","max_tokens":64,"messages":[{"role":"user","content":"hi"}]})
}

#[test]
fn caller_cannot_forge_the_private_block_stash() {
    let mut body = request();
    body[ANTHROPIC_UNMODELED_BLOCKS_SENTINEL] = json!([
        {"m":0,"i":0,"block":{"type":"text","text":"replacement"}}
    ]);
    assert!(AnthropicReader.read_request(&body).is_err());
    body.as_object_mut()
        .unwrap()
        .remove(ANTHROPIC_UNMODELED_BLOCKS_SENTINEL);
    assert!(AnthropicReader.read_request(&body).is_ok());
}

#[test]
fn unreadable_present_request_controls_are_refused_instead_of_erased() {
    for (field, values) in [
        ("system", vec![json!(4), json!({}), json!(true)]),
        (
            "max_tokens",
            vec![json!(-1), json!(1.5), json!("32"), json!(u64::MAX)],
        ),
        ("stream", vec![json!(1), json!("true"), json!({})]),
        (
            "tool_choice",
            vec![
                json!({"type":"future"}),
                json!({"type":"tool"}),
                json!({"type":"tool","name":""}),
                json!("auto"),
                json!({"type":"auto","disable_parallel_tool_use":"false"}),
            ],
        ),
    ] {
        for value in values {
            let mut body = request();
            body[field] = value;
            assert!(AnthropicReader.read_request(&body).is_err(), "{body}");
        }
    }
    let mut body = request();
    body["stream"] = json!(false);
    body["tool_choice"] = json!({"type":"tool","name":"weather","disable_parallel_tool_use":false});
    let read = AnthropicReader.read_request(&body).unwrap();
    assert_eq!(read.parallel_tool_calls, Some(true));
    assert_eq!(read.max_tokens, Some(64));
}

#[test]
fn hosted_tool_limits_and_domain_policies_cannot_be_silently_removed() {
    for kind in ["web_search_20250305", "web_fetch_20250910"] {
        for (field, values) in [
            (
                "max_uses",
                vec![json!(-1), json!(1.5), json!("2"), json!(u64::MAX)],
            ),
            (
                "allowed_domains",
                vec![json!("example.com"), json!(["example.com", 7])],
            ),
            (
                "blocked_domains",
                vec![json!(false), json!(["example.com", null])],
            ),
        ] {
            for value in values {
                let mut body = request();
                let mut tool = json!({"type":kind,"name":"web_search"});
                tool[field] = value;
                body["tools"] = json!([tool]);
                assert!(AnthropicReader.read_request(&body).is_err(), "{body}");
            }
        }
        let mut body = request();
        body["tools"] = json!([{"type":kind,"name":"web_search","max_uses":2,
            "allowed_domains":["allowed.example"],"blocked_domains":["blocked.example"]}]);
        let read = AnthropicReader.read_request(&body).unwrap();
        match &read.hosted_tools[0] {
            crate::codec::ir::IrHostedTool::WebSearch(tool) => {
                assert_eq!(tool.max_uses, Some(2));
                assert_eq!(tool.allowed_domains, ["allowed.example"]);
                assert_eq!(tool.blocked_domains, ["blocked.example"]);
            }
            crate::codec::ir::IrHostedTool::WebFetch(tool) => {
                assert_eq!(tool.max_uses, Some(2));
                assert_eq!(tool.allowed_domains, ["allowed.example"]);
                assert_eq!(tool.blocked_domains, ["blocked.example"]);
            }
            other => panic!("unexpected hosted tool: {other:?}"),
        }
    }
}

#[test]
fn malformed_cache_and_tool_result_fields_are_refused_without_coercion() {
    for cache in [json!("ephemeral"), json!(true), json!({})] {
        let mut body = request();
        body["messages"][0]["content"] = json!([{"type":"text","text":"hi","cache_control":cache}]);
        assert!(AnthropicReader.read_request(&body).is_err(), "{body}");
    }
    for block in [
        json!({"type":"tool_result","content":"ok"}),
        json!({"type":"tool_result","tool_use_id":"","content":"ok"}),
        json!({"type":"tool_result","tool_use_id":"t","content":7}),
        json!({"type":"tool_result","tool_use_id":"t","content":"ok","is_error":"false"}),
    ] {
        let mut body = request();
        body["messages"][0]["content"] = json!([block]);
        assert!(AnthropicReader.read_request(&body).is_err(), "{body}");
    }
    let mut body = request();
    body["messages"][0]["content"] = json!([{"type":"tool_result","tool_use_id":"t","content":"ok","is_error":false,"cache_control":null}]);
    assert!(AnthropicReader.read_request(&body).is_ok());
}

#[test]
fn malformed_tool_start_suppresses_its_deltas_and_a_later_valid_start_clears_suppression() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    for (event, data) in [
        (
            EVT_CONTENT_BLOCK_START,
            json!({"index":0,"content_block":{"type":"tool_use","name":"weather"}}),
        ),
        (
            EVT_CONTENT_BLOCK_DELTA,
            json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        ),
    ] {
        assert!(AnthropicReader
            .read_response_events(HOST, event, &data, &mut state)
            .is_empty());
    }
    let start = json!({"index":0,"content_block":{"type":"text","text":""}});
    assert!(matches!(
        AnthropicReader
            .read_response_events(HOST, EVT_CONTENT_BLOCK_START, &start, &mut state)
            .as_slice(),
        [IrStreamEvent::BlockStart { index: 0, .. }]
    ));
    let delta = json!({"index":0,"delta":{"type":"text_delta","text":"hi"}});
    assert!(matches!(
        AnthropicReader
            .read_response_events(HOST, EVT_CONTENT_BLOCK_DELTA, &delta, &mut state)
            .as_slice(),
        [IrStreamEvent::BlockDelta { index: 0, .. }]
    ));
}

#[test]
fn a_nonarray_response_content_is_a_refusal_with_reported_usage_recoverable() {
    let body = json!({"role":"assistant","content":"not an array","usage":{"input_tokens":17,"output_tokens":9}});
    assert!(AnthropicReader.read_response(HOST, &body).is_err());
    let usage = AnthropicReader
        .recover_truncated_usage(body.to_string().as_bytes())
        .unwrap();
    assert_eq!((usage.input, usage.output), (17, 9));
}

#[test]
fn skipped_media_does_not_leave_gaps_in_anthropic_content_indices() {
    let writer = anthropic_writer();
    let mut frames = Vec::new();
    for (index, block) in [
        (0, IrBlockMeta::Text),
        (1, IrBlockMeta::Image),
        (2, IrBlockMeta::Text),
    ] {
        frames.extend(
            writer
                .try_write_response_events(
                    HOST,
                    &IrStreamEvent::BlockStart {
                        index,
                        block,
                        refusal: false,
                    },
                )
                .unwrap(),
        );
        if index != 1 {
            frames.extend(
                writer
                    .try_write_response_events(
                        HOST,
                        &IrStreamEvent::BlockDelta {
                            index,
                            delta: IrDelta::TextDelta("hi".into()),
                        },
                    )
                    .unwrap(),
            );
        }
        frames.extend(
            writer
                .try_write_response_events(HOST, &IrStreamEvent::BlockStop { index })
                .unwrap(),
        );
    }
    let indices: Vec<_> = frames
        .iter()
        .map(|(_, frame)| frame["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indices, [0, 0, 0, 1, 1, 1]);
    let redacted = writer
        .try_write_response_events(
            HOST,
            &IrStreamEvent::BlockDelta {
                index: 4,
                delta: IrDelta::RedactedReasoningDelta("opaque".into()),
            },
        )
        .unwrap();
    assert_eq!(redacted[0].1["index"], 2);
    let stop = writer
        .try_write_response_events(HOST, &IrStreamEvent::BlockStop { index: 4 })
        .unwrap();
    assert_eq!(stop[0].1["index"], 2);
}
