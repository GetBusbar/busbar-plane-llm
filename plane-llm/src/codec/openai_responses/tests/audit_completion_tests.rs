use super::*;
use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent, StreamDecodeState};

#[test]
fn tool_arguments_finalize_once_before_the_item_with_matching_identity() {
    let host = crate::codec::test_host::HOST;
    let writer = ResponsesWriter;
    let added = writer.write_response_events(
        host,
        &IrStreamEvent::BlockStart {
            index: 3,
            block: IrBlockMeta::ToolUse {
                id: "call_weather".into(),
                name: "weather".into(),
            },
            refusal: false,
        },
    );
    for fragment in ["{\"city\":", "\"Paris\"}"] {
        writer.write_response_events(
            host,
            &IrStreamEvent::BlockDelta {
                index: 3,
                delta: IrDelta::InputJsonDelta(fragment.into()),
            },
        );
    }
    let done = writer.write_response_events(host, &IrStreamEvent::BlockStop { index: 3 });
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].0, "response.function_call_arguments.done");
    assert_eq!(done[0].1["type"], done[0].0);
    assert_eq!(done[0].1["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(done[0].1["output_index"], 3);
    assert_eq!(done[0].1["item_id"], added[0].1["item"]["id"]);
    assert_eq!(done[1].0, "response.output_item.done");
    assert_eq!(done[1].1["item"]["arguments"], done[0].1["arguments"]);
    assert_eq!(done[1].1["item"]["call_id"], "call_weather");
    assert_eq!(done[1].1["item"]["name"], "weather");
    assert_eq!(
        done[1].1["sequence_number"].as_u64().unwrap(),
        done[0].1["sequence_number"].as_u64().unwrap() + 1
    );
    assert!(writer
        .write_response_events(host, &IrStreamEvent::BlockStop { index: 3 })
        .is_empty());
}

#[test]
fn quota_exhaustion_is_billing_in_buffered_and_streamed_failures() {
    for (signal, expected) in [
        ("insufficient_quota", StatusClass::Billing),
        ("rate_limit_exceeded", StatusClass::RateLimit),
    ] {
        for field in ["code", "type"] {
            let failed = serde_json::json!({
                "status":"failed", "output":[],
                "error":{(field):signal,"message":"provider failure"}
            });
            let error = ResponsesReader
                .read_response(crate::codec::test_host::HOST, &failed)
                .unwrap_err();
            assert_eq!(error.class, expected);
            let events = reader_read_response_events(
                EVT_RESPONSE_FAILED,
                &serde_json::json!({"response":failed}),
                &mut StreamDecodeState::default(),
            );
            let IrStreamEvent::Error(error) = &events[0] else {
                panic!("expected provider error, got {:?}", events[0]);
            };
            assert_eq!(error.class, expected);
            assert_eq!(error.provider_signal.as_deref(), Some(signal));
        }
    }
}
