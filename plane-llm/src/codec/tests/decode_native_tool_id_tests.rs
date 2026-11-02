// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `decode_native_tool_id`'s lowercase-hex guard. `native_for` only ever emits a lowercase-hex tail,

use crate::codec::proto_codec::{decode_native_tool_id, ToolIdRemap, TOOL_ID_REMAP_MAX_MEMO};

#[test]
fn genuine_lowercase_busbar_id_still_decodes() {
    let mut remap = ToolIdRemap::default();
    let native = remap.native_for("anthropic", "call_abc123");

    assert!(native.starts_with("toolu_bb1"));
    assert_eq!(
        native.strip_prefix("toolu_bb1").unwrap(),
        native
            .strip_prefix("toolu_bb1")
            .unwrap()
            .to_ascii_lowercase()
    );
    assert_eq!(
        decode_native_tool_id("anthropic", &native).as_deref(),
        Some("call_abc123")
    );
}

#[test]
fn uppercase_hex_client_id_passes_through_verbatim() {
    assert_eq!(
        decode_native_tool_id("anthropic", "toolu_bb143616C6C"),
        None
    );

    assert_eq!(
        decode_native_tool_id("anthropic", "toolu_bb143616c6C"),
        None
    );
}

/// The memo is an OPTIMIZATION over a pure, deterministic encode, and it is fed from UNTRUSTED
#[test]
fn memo_is_capped_and_answers_identically_past_the_cap() {
    let mut remap = ToolIdRemap::default();
    for i in 0..(TOOL_ID_REMAP_MAX_MEMO + 500) {
        let _ = remap.native_for("anthropic", &format!("call_{i}"));
    }
    assert!(
        remap.memo_len() <= TOOL_ID_REMAP_MAX_MEMO,
        "memo must be capped at {TOOL_ID_REMAP_MAX_MEMO}, retained {}",
        remap.memo_len()
    );

    let past_cap = format!("call_{}", TOOL_ID_REMAP_MAX_MEMO + 400);
    let native = remap.native_for("anthropic", &past_cap);
    assert_eq!(
        native,
        ToolIdRemap::default().native_for("anthropic", &past_cap),
        "an uncached id past the cap must map to the same deterministic native id"
    );
    assert_eq!(
        remap.native_for("anthropic", &past_cap),
        native,
        "repeat lookups past the cap stay stable"
    );
    assert_eq!(
        decode_native_tool_id("anthropic", &native).as_deref(),
        Some(past_cap.as_str()),
        "an uncached native id still reverses to the original egress id"
    );
}
