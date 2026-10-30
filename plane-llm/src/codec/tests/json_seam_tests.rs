// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for the plane's JSON seam (`json.rs`): every parse goes through the depth guard.

use super::*;

#[test]
fn rejects_pathologically_nested_input_without_overflow() {
    // A ~2 MB body 1,000,000 arrays deep would abort the process on re-serialize/drop if it reached
    // `from_slice`. The guard rejects it on the raw bytes first, so this returns Err cleanly.
    let depth = 1_000_000usize;
    let s = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(
        parse::<serde_json::Value>(s.as_bytes()).is_err(),
        "deeply-nested body must be rejected"
    );
    assert!(exceeds_max_depth(s.as_bytes(), MAX_JSON_DEPTH));
}

#[test]
fn accepts_realistic_depth_and_counts_correctly() {
    let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hi [bracket] {brace} in a string is not depth"}]}]}"#;
    assert!(!exceeds_max_depth(body, MAX_JSON_DEPTH));
    assert!(parse::<serde_json::Value>(body).is_ok());
    assert!(!exceeds_max_depth(br#"{"k":"[[[[[[[[[[ {{{{{{ ]]]]]"}"#, 8));
    let at_limit = format!("{}{}", "[".repeat(128), "]".repeat(128));
    assert!(!exceeds_max_depth(at_limit.as_bytes(), MAX_JSON_DEPTH));
    let over = format!("{}{}", "[".repeat(129), "]".repeat(129));
    assert!(exceeds_max_depth(over.as_bytes(), MAX_JSON_DEPTH));
}
