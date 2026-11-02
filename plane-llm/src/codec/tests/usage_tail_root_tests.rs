use super::isolate_tail_usage_object;
use serde_json::json;

#[test]
fn only_a_complete_response_root_member_can_supply_usage() {
    for bytes in [
        br#"{"tool":{"usage":{"prompt_tokens":999}}}"#.as_slice(),
        br#"{"tool":{"usage":{"prompt_tokens":999}}"#.as_slice(),
        br#""usage":{"prompt_tokens":999}"#.as_slice(),
        br#"[{"usage":{"prompt_tokens":999}}]"#.as_slice(),
        br#"{"tool":garbage,"usage":{"prompt_tokens":999}}"#.as_slice(),
        br#"{"text":"\"usage\":{\"prompt_tokens\":999}"}"#.as_slice(),
    ] {
        assert!(
            isolate_tail_usage_object(bytes, b"\"usage\"").is_none(),
            "{bytes:?}"
        );
    }
    for bytes in [
        br#"{"tool":{"usage":{"prompt_tokens":999}},"usage":{"prompt_tokens":7},"after":"#
            .as_slice(),
        br#"{"usage":{"prompt_tokens":7},"tool":{"usage":{"prompt_tokens":999}},"after":"#
            .as_slice(),
        br#"{"u\u0073age":{"prompt_tokens":7},"after":"#.as_slice(),
    ] {
        assert_eq!(
            isolate_tail_usage_object(bytes, b"\"usage\""),
            Some(json!({"prompt_tokens":7}))
        );
    }
}

#[test]
fn duplicate_root_usage_has_the_same_last_member_custody_as_json() {
    assert_eq!(
        isolate_tail_usage_object(
            br#"{"usage":{"prompt_tokens":7},"usage":{"prompt_tokens":8},"after":"#,
            b"\"usage\""
        ),
        Some(json!({"prompt_tokens":8}))
    );
    for bytes in [
        br#"{"usage":{"prompt_tokens":7},"usage":null}"#.as_slice(),
        br#"{"usage":{"prompt_tokens":7},"usage":{"prompt_tokens":"#.as_slice(),
    ] {
        assert!(isolate_tail_usage_object(bytes, b"\"usage\"").is_none());
    }
}
