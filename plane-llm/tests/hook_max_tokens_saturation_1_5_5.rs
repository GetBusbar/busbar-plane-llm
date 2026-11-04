// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The size signal a hook sees for an output cap above `u32::MAX`, as 1.5.5 projected it.

use busbar_plane_llm::exchange::arrive::arrive;
use busbar_plane_llm::exchange::project::{max_tokens_for, project};

fn hook_max_tokens(target: &str, body: serde_json::Value) -> Option<u32> {
    let bytes = serde_json::to_vec(&body).expect("body serializes");
    let fields: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let arrived = arrive("POST", target, fields, &bytes, &()).expect("the request arrives");
    project(&arrived).max_tokens
}

#[test]
fn an_anthropic_cap_above_u32_saturates() {
    let body = serde_json::json!({
        "model": "m0",
        "max_tokens": 5_000_000_000u64,
        "messages": [{"role": "user", "content": "hi"}]
    });
    assert_eq!(hook_max_tokens("/v1/messages", body), Some(u32::MAX));
}

#[test]
fn an_openai_cap_above_u32_saturates() {
    let body = serde_json::json!({
        "model": "m0",
        "max_tokens": u64::MAX,
        "messages": [{"role": "user", "content": "hi"}]
    });
    assert_eq!(
        hook_max_tokens("/v1/chat/completions", body),
        Some(u32::MAX)
    );
}

#[test]
fn the_cap_is_read_under_the_dialects_own_key() {
    let responses =
        serde_json::json!({"input": "hi", "max_tokens": 999, "max_output_tokens": 4096});
    assert_eq!(max_tokens_for(&responses, "responses"), Some(4096));
    let anthropic = serde_json::json!({"max_tokens": 512});
    assert_eq!(max_tokens_for(&anthropic, "anthropic"), Some(512));
    assert_eq!(max_tokens_for(&anthropic, "gemini"), Some(512));
    assert_eq!(
        max_tokens_for(&serde_json::json!({"max_tokens": u64::MAX}), "anthropic"),
        Some(u32::MAX)
    );
}

#[test]
fn projected_caps_include_openai_completion_and_gemini_generation_config() {
    use busbar_plane_llm::exchange::project::max_tokens_for;
    assert_eq!(
        max_tokens_for(&serde_json::json!({"max_completion_tokens":1000}), "openai"),
        Some(1000)
    );
    assert_eq!(
        max_tokens_for(
            &serde_json::json!({"max_tokens":20,"max_completion_tokens":1000}),
            "openai"
        ),
        Some(20)
    );
    assert_eq!(
        max_tokens_for(
            &serde_json::json!({"generationConfig":{"maxOutputTokens":1000}}),
            "gemini"
        ),
        Some(1000)
    );
    assert_eq!(
        max_tokens_for(
            &serde_json::json!({"generationConfig":{"maxOutputTokens":u64::MAX}}),
            "gemini"
        ),
        Some(u32::MAX)
    );
}
