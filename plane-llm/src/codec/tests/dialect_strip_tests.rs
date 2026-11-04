// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `strip_top_level_usage_member`'s refusal contract, and the dialect helpers' answers to
//! the shared fixture (moved with the host's copies when those were deleted, SD-8).

use super::*;

/// A TRUNCATED top-level object must be refused, not spliced.
#[test]
fn a_truncated_object_is_refused_rather_than_spliced() {
    for input in [
        r#"{"a":1,"usage":{"x":1}"#,
        r#"{"usage":null,"a":1"#,
        r#"{"usage":{"prompt_tokens":3}"#,
    ] {
        let out = strip_top_level_usage_member(input);
        assert!(
            out.is_none(),
            "a truncated object must fall back, got {out:?} for {input}"
        );
    }
}

/// The refusal above must not have cost the well-formed case: a CLOSED object with a top-level
/// `usage` still strips, byte-for-byte, and the result is parseable.
#[test]
fn a_closed_object_still_strips_and_stays_parseable() {
    let out = strip_top_level_usage_member(r#"{"a":1,"usage":{"x":1},"b":2}"#)
        .expect("a well-formed object still strips");
    assert_eq!(out, r#"{"a":1,"b":2}"#);
    serde_json::from_str::<serde_json::Value>(&out).expect("the stripped object parses");

    let first = strip_top_level_usage_member(r#"{"usage":null,"a":1}"#).expect("first member");
    assert_eq!(first, r#"{"a":1}"#);
}
