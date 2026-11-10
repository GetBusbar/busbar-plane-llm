use super::*;
#[test]
fn responses_usage_preserves_read_and_write_cache_categories() {
    let wire = serde_json::json!({"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":20,"cache_write_tokens":30}});
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert_eq!(usage.input_tokens, 50);
    assert_eq!(usage.cache_read_input_tokens, Some(20));
    assert_eq!(usage.cache_creation_input_tokens, Some(30));
    assert_eq!(usage.output_tokens, 7);
}

fn split_usage() -> serde_json::Value {
    serde_json::json!({
        "input_tokens": 12000,
        "input_tokens_details": {
            "cached_tokens": 4000, "cache_write_tokens": 7000,
            "cache_write_tokens_details": {"ttl_30m_tokens": 3000, "ttl_12h_tokens": 4000}
        },
        "output_tokens": 10, "total_tokens": 12010
    })
}

#[test]
fn split_cache_writes_are_disjoint_in_the_live_door_and_wire_totals() {
    let wire = split_usage();
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert!(usage.detail.refused_counts.is_empty());
    assert_eq!(usage.input_tokens, 1000);
    assert_eq!(usage.cache_creation_input_tokens, Some(7000));
    assert_eq!(usage.detail.cache_creation_30m_input_tokens, Some(3000));
    assert_eq!(usage.detail.cache_creation_12h_input_tokens, Some(4000));
    assert!(usage.detail.usage_identity_note.is_none());
    let token_usage = usage.to_token_usage();
    let exact = token_usage.exact_counts.as_ref().unwrap();
    assert!(!exact.contains_key("cache_write"));
    assert!(!exact.contains_key("unitemized_tokens"));
    let counts = crate::plane_door::counts(&crate::exchange::reply::Units::of(
        Some(&token_usage),
        Default::default(),
    ));
    // Historical classes and the request fee keep their indices; the new classes follow the fee.
    assert_eq!(crate::plane_door::FEE_CLASS_INDEX, 19);
    assert_eq!(
        counts
            .iter()
            .map(|u| (u.class, u.amount))
            .collect::<Vec<_>>(),
        vec![(0, 1000), (1, 10), (2, 4000), (20, 3000), (21, 4000)]
    );
    let restored = build_responses_usage(&usage);
    assert_eq!(restored["input_tokens"], 12000);
    assert_eq!(restored["total_tokens"], 12010);
    assert_eq!(
        restored["input_tokens_details"],
        wire["input_tokens_details"]
    );
    // All translated dialects use these shared exact aggregate accessors.
    assert_eq!(usage.wire_count("cache_write", 0), 7000);
    assert_eq!(
        usage.wire_total(&["input", "cache_read", "cache_write"], 0),
        12000
    );
    assert_eq!(
        crate::codec::usage_count::wire_token_total(&token_usage),
        12010
    );
}

#[test]
fn split_cache_writes_preserve_explicit_zero_and_fractional_exact_counts() {
    let mut wire = split_usage();
    wire["input_tokens_details"]["cache_write_tokens_details"] =
        serde_json::json!({"ttl_30m_tokens":0,"ttl_12h_tokens":7000});
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert_eq!(usage.detail.cache_creation_30m_input_tokens, Some(0));
    assert_eq!(
        build_responses_usage(&usage)["input_tokens_details"]["cache_write_tokens_details"]
            ["ttl_30m_tokens"],
        0
    );
    wire["input_tokens_details"]["cache_write_tokens_details"] =
        serde_json::json!({"ttl_30m_tokens":0.125,"ttl_12h_tokens":6999.875});
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert!(usage.detail.refused_counts.is_empty());
    assert_eq!(
        build_responses_usage(&usage)["input_tokens_details"],
        wire["input_tokens_details"]
    );
}

#[test]
fn invalid_cache_partitions_are_unresolved_without_cheap_fallback() {
    for split in [
        serde_json::json!({"ttl_30m_tokens":3000}),
        serde_json::json!({"ttl_30m_tokens":3000,"ttl_12h_tokens":null}),
        serde_json::json!({"ttl_30m_tokens":3000,"ttl_12h_tokens":3999}),
        serde_json::json!({"ttl_30m_tokens":-1,"ttl_12h_tokens":7001}),
        serde_json::json!({"ttl_30m_tokens":"3000","ttl_12h_tokens":4000}),
        serde_json::json!({"ttl_30m_tokens":18446744073709551615u64,"ttl_12h_tokens":4000}),
        serde_json::Value::Null,
        serde_json::json!([]),
    ] {
        let mut wire = split_usage();
        wire["input_tokens_details"]["cache_write_tokens_details"] = split;
        let usage = read_responses_usage(Some(&wire), None).unwrap();
        assert!(!usage.detail.refused_counts.is_empty(), "{wire}");
        assert_eq!(
            usage
                .detail
                .exact_counts
                .as_ref()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["output"]
        );
        assert_eq!(
            build_responses_usage(&usage)["input_tokens"],
            serde_json::Value::Null
        );
    }
    let mut wire = split_usage();
    wire["input_tokens"] = serde_json::json!(10999);
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert!(!usage.detail.refused_counts.is_empty());
    assert!(!usage
        .detail
        .exact_counts
        .as_ref()
        .unwrap()
        .contains_key("input"));
}

#[test]
fn split_cache_writes_are_identical_buffered_streamed_and_recovered() {
    use crate::codec::ir::{IrStreamEvent, StreamDecodeState};
    let wire = split_usage();
    let body = serde_json::json!({"id":"resp_split", "object":"response", "created_at":1, "status":"completed", "model":"cache-model", "output":[], "usage":wire});
    let buffered = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &body)
        .unwrap()
        .usage;
    let streamed = ResponsesReader
        .read_response_events(
            crate::codec::test_host::HOST,
            EVT_RESPONSE_COMPLETED,
            &serde_json::json!({"response":body}),
            &mut StreamDecodeState::default(),
        )
        .into_iter()
        .find_map(|event| match event {
            IrStreamEvent::MessageDelta { usage, .. } => Some(usage),
            _ => None,
        })
        .unwrap();
    let recovered = ResponsesReader
        .recover_truncated_usage(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(buffered.detail.exact_counts, streamed.detail.exact_counts);
    assert_eq!(buffered.detail.exact_counts, recovered.exact_counts);
    assert!(recovered.refused_counts.is_empty());
}
