// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::RootUsage;

fn project(bytes: &[u8], cap: usize, chunk: usize) -> Option<serde_json::Value> {
    let mut scan = RootUsage::new(cap);
    for piece in bytes.chunks(chunk) {
        scan.feed(piece);
    }
    serde_json::from_slice(&scan.finish()?).ok()
}

#[test]
fn original_root_members_survive_large_values_and_every_chunk_boundary() {
    let body = format!(
        r#"{{"data":[{{"text":"{}é🦀\uD83E\uDD80","usage":{{"prompt_tokens":999}}}}],"us\u0061ge":{{"prompt_tokens":5}},"meta":{{"billed_units":{{"search_units":2}}}}}}"#,
        "x".repeat(2000)
    );
    for size in [1, 2, 3, 7, 64, body.len()] {
        let value = project(body.as_bytes(), 64, size).unwrap();
        assert_eq!(value["usage"]["prompt_tokens"], 5);
        assert_eq!(value["meta"]["billed_units"]["search_units"], 2);
        assert!(value.get("data").is_none());
    }
}

#[test]
fn malformed_original_prefix_and_headless_or_string_spoofs_cannot_bill() {
    for bytes in [
        br#""usage":{"prompt_tokens":99}}"#.as_slice(),
        br#"{"data":[1,,2],"usage":{"prompt_tokens":99}}"#,
        br#"{"data":{"x":1],"usage":{"prompt_tokens":99}}"#,
        br#"{"data":"bad\xescape","usage":{"prompt_tokens":99}}"#,
        br#"{"data":"\uD800","usage":{"prompt_tokens":99}}"#,
        br#"{"data":true false,"usage":{"prompt_tokens":99}}"#,
        br#"{"usage":{"prompt_tokens":99}} trailing"#,
        br#"{"usage":{"prompt_tokens":99}"#,
    ] {
        assert!(project(bytes, 64, 1).is_none(), "{bytes:?}");
    }
    let value = project(
        br#"{"data":"\"usage\":{\"prompt_tokens\":99}","nested":{"usage":{"prompt_tokens":99}}}"#,
        64,
        1,
    )
    .unwrap();
    assert!(value.get("usage").is_none());
}

#[test]
fn duplicate_oversized_members_invalidate_prior_counts_and_memory_stays_bounded() {
    let body = format!(
        r#"{{"usage":{{"prompt_tokens":5}},"usage":{{"padding":"{}"}},"data":[{}]}}"#,
        "x".repeat(10000),
        vec!["0"; 10000].join(",")
    );
    let mut scan = RootUsage::new(64);
    for chunk in body.as_bytes().chunks(1024) {
        scan.feed(chunk);
        assert!(scan.capture.len() <= 64);
        assert!(scan.key_bytes.len() <= 129);
        assert!(scan.scalar.len() <= 128);
        assert!(scan.character.len() <= 12);
        assert!(scan.stack.len() <= 128);
    }
    let value: serde_json::Value = serde_json::from_slice(&scan.finish().unwrap()).unwrap();
    assert!(value.get("usage").is_none());
    let deep = format!(
        r#"{{"data":{}0{},"usage":{{"prompt_tokens":5}}}}"#,
        "[".repeat(129),
        "]".repeat(129)
    );
    assert!(project(deep.as_bytes(), 64, 1).is_none());
}

#[test]
fn failure_discriminators_are_retained_or_fail_closed() {
    let value = project(
        br#"{"status":"failed","error":{"message":"no"},"usage":{"prompt_tokens":5}}"#,
        64,
        1,
    )
    .unwrap();
    assert_eq!(value["status"], "failed");
    let large = format!(
        r#"{{"error":"{}","usage":{{"prompt_tokens":5}}}}"#,
        "x".repeat(100)
    );
    assert!(project(large.as_bytes(), 64, 1).is_none());
}

#[test]
fn titan_root_count_is_retained_without_nested_or_string_spoofs() {
    let body = format!(
        r#"{{"embedding":[{}],"inputTextTokenCount":5}}"#,
        vec!["0.25"; 200].join(",")
    );
    for chunk in [1, 2, 7, 128, body.len()] {
        assert_eq!(
            project(body.as_bytes(), 128, chunk).unwrap(),
            serde_json::json!({"inputTextTokenCount": 5})
        );
    }
    for body in [
        br#"{"nested":{"inputTextTokenCount":999},"text":"\"inputTextTokenCount\":999"}"#
            .as_slice(),
        br#"{"inputTextTokenCount":5,"inputTextTokenCount":null}"#,
    ] {
        let value = project(body, 128, 1).unwrap();
        assert!(value
            .get("inputTextTokenCount")
            .is_none_or(serde_json::Value::is_null));
    }
    assert!(project(br#"{"embedding":[1,,2],"inputTextTokenCount":5}"#, 128, 1).is_none());
}

#[test]
fn responses_projection_drops_large_payloads_keeps_metadata_at_each_boundary() {
    let body = format!(
        r#"{{"output":[{{"id":"ig_1","type":"image_generation_call","status":"completed","result":"{}é🦀\uD83E\uDD80"}},{{"id":"sh_1","type":"shell_call","status":"completed","environment":{{"type":"container_reference","container_id":"cntr_1"}},"action":{{"commands":["{}"]}}}}],"tool_usage":{{"image_gen":{{"input_tokens":2}}}},"usage":{{"input_tokens":7}}}}"#,
        "x".repeat(5000),
        "x".repeat(5000)
    );
    for size in [1, 2, 3, 7, 64, body.len()] {
        let mut scan = RootUsage::new(512);
        for piece in body.as_bytes().chunks(size) {
            scan.feed(piece);
            assert!(scan.capture.len() <= 512);
            assert!(scan.output_bytes <= 512);
            if let Some(child) = &scan.output_item {
                assert!(child.capture.len() <= 512);
            }
        }
        let projected = scan.finish().unwrap();
        assert!(projected.len() < 512);
        let value: serde_json::Value = serde_json::from_slice(&projected).unwrap();
        assert_eq!(value["usage"]["input_tokens"], 7);
        assert_eq!(value["tool_usage"]["image_gen"]["input_tokens"], 2);
        assert_eq!(value["output"][0]["id"], "ig_1");
        assert_eq!(value["output"][1]["environment"]["container_id"], "cntr_1");
        assert!(value["output"][0].get("result").is_none());
        assert!(value["output"][1].get("action").is_none());
    }
}
#[test]
fn responses_projection_ambiguous_tool_metadata_preserves_parent_usage() {
    for body in [
        r#"{"usage":{"input_tokens":7},"tool_usage":{"image_gen":{"input_tokens":1,"input_tokens":2}}}"#,
        r#"{"usage":{"input_tokens":7},"tool_usage":{},"tool_usage":{}}"#,
        r#"{"usage":{"input_tokens":7},"output":[],"output":[]}"#,
        r#"{"usage":{"input_tokens":7},"output":[{"id":"a","id":"b","type":"image_generation_call","status":"completed"}]}"#,
    ] {
        let value = project(body.as_bytes(), 512, 1).unwrap();
        assert_eq!(value["usage"]["input_tokens"], 7);
        assert!(
            value.get("tool_usage") == Some(&serde_json::Value::Bool(false))
                || value.get("output") == Some(&serde_json::Value::Bool(false))
                || value["output"][0]["id"] == serde_json::Value::Bool(false)
        );
    }
}
#[test]
fn responses_projection_metadata_overflow_cannot_become_an_empty_call_list() {
    let body = format!(
        r#"{{"usage":{{"input_tokens":7}},"output":[{{"id":"{}","type":"image_generation_call","status":"completed"}}]}}"#,
        "x".repeat(4000)
    );
    let value = project(body.as_bytes(), 128, 7).unwrap();
    assert_eq!(value["usage"]["input_tokens"], 7);
    assert!(
        value["output"] == serde_json::Value::Bool(false)
            || value["output"][0]["id"] == serde_json::Value::Bool(false)
    );
}
