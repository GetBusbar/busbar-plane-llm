// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! CROSS-PROTOCOL extra-key round-trip tests, RELOCATED here from `busbar-core`'s `proxy/tests/`

use super::*;

/// Structural class fix: on a CROSS-protocol request hop the source-protocol-only passthrough

#[test]
fn cross_protocol_strips_source_only_extra_keys() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 16,
        "logprobs": true,
        "top_logprobs": 5,
        "logit_bias": {"50256": -100}
    });
    let openai = Protocol::openai();
    let mut ir = openai.reader().read_request(&body).expect("read");

    assert!(ir.extra.contains_key("logit_bias"));
    assert!(!ir.extra.contains_key("logprobs"));
    assert!(!ir.extra.contains_key("top_logprobs"));
    assert_eq!(ir.logprobs, Some(true));
    assert_eq!(ir.top_logprobs, Some(5));

    ir.extra.clear();

    let anthropic = Protocol::anthropic();
    let out = anthropic
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let obj = out.as_object().expect("object body");
    assert!(
        !obj.contains_key("logprobs"),
        "logprobs must not leak onto an Anthropic backend body"
    );
    assert!(!obj.contains_key("top_logprobs"));
    assert!(!obj.contains_key("logit_bias"));
    assert!(obj.contains_key("messages"));

    let gemini = Protocol::gemini();
    let gout = gemini
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        gout["generationConfig"]["responseLogprobs"],
        serde_json::json!(true),
        "the logprobs ask must carry to Gemini as responseLogprobs"
    );
    assert_eq!(gout["generationConfig"]["logprobs"], serde_json::json!(5));
}

/// SAME-protocol openai→openai must still carry `logprobs` (now via the first-class IR field

#[test]
fn same_protocol_passthrough_preserves_extra_keys() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "logprobs": true,
        "logit_bias": {"50256": -100},
        "n": 3
    });
    let openai = Protocol::openai();
    let ir = openai.reader().read_request(&body).expect("read");

    let out = openai
        .writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let obj = out.as_object().expect("object body");
    assert_eq!(
        obj.get("logprobs"),
        Some(&serde_json::json!(true)),
        "same-protocol openai→openai must preserve logprobs"
    );
    assert_eq!(
        obj.get("logit_bias"),
        Some(&serde_json::json!({"50256": -100}))
    );
    assert_eq!(obj.get("n"), Some(&serde_json::json!(3)));
}
