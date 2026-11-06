use super::*;

#[test]
fn served_tier_preserves_unknown_and_reported_values_in_every_usage_envelope() {
    use serde_json::{json, Value};

    assert_eq!(usage::write_usage_object(None)["service_tier"], Value::Null);
    for (reported, expected) in [
        (None, Value::Null),
        (Some(Value::Null), Value::Null),
        (Some(json!("flex")), Value::Null),
        (Some(json!("scale")), Value::Null),
        (Some(json!("standard")), json!("standard")),
        (Some(json!("priority")), json!("priority")),
        (Some(json!("batch")), json!("batch")),
    ] {
        let mut wire = json!({"input_tokens": 7, "output_tokens": 3});
        if let Some(tier) = reported {
            wire["service_tier"] = tier;
        }
        let read = usage::read_anthropic_usage(Some(&wire)).expect("usage reads");
        let response = crate::codec::ir::IrResponse {
            role: crate::codec::ir::IrRole::Assistant,
            usage: read.clone(),
            ..Default::default()
        };
        let writer = AnthropicWriter;
        let buffered = writer.write_response(crate::codec::test_host::HOST, &response);
        let start = writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::MessageStart {
                    role: crate::codec::ir::IrRole::Assistant,
                    usage: Some(read.clone()),
                    id: None,
                    created: None,
                    model: None,
                },
            )
            .expect("message start");
        let delta = usage::write_message_delta_usage(&read);
        for full in [&buffered["usage"], &start.1["message"]["usage"]] {
            assert_eq!(full["service_tier"], expected, "{wire}");
            assert_eq!(full["input_tokens"], 7);
            assert_eq!(full["output_tokens"], 3);
        }
        assert_eq!(delta["input_tokens"], 7);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(
            delta.get("service_tier"),
            (!expected.is_null()).then_some(&expected),
            "message_delta omits an unknown tier, preserving all reported counts"
        );
    }
}

#[test]
fn server_tool_usage_preserves_fetch_counts_in_both_envelopes() {
    for (search, fetch) in [(None, Some(3)), (Some(2), Some(3)), (Some(2), None)] {
        let mut usage = crate::codec::ir::IrUsage::default();
        usage.detail.web_search_requests = search;
        usage.detail.web_fetch_requests = fetch;
        for wire in [
            usage::write_usage_object(Some(&usage)),
            usage::write_message_delta_usage(&usage),
        ] {
            assert_eq!(
                wire["server_tool_use"]["web_search_requests"],
                search.unwrap_or(0)
            );
            assert_eq!(
                wire["server_tool_use"]["web_fetch_requests"],
                fetch.unwrap_or(0)
            );
        }
    }
    assert!(usage::write_server_tool_use(&crate::codec::ir::IrUsage::default()).is_null());
}
