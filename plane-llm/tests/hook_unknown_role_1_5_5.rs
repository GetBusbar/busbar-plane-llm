// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! A TURN WHOSE ROLE NO READER KNOWS is screened and the request goes on, as in 1.5.5.

use busbar_plane_llm::exchange::arrive::arrive;
use busbar_plane_llm::exchange::project::project;

fn arrived(target: &str, body: &serde_json::Value) -> busbar_plane_llm::exchange::arrive::Arrived {
    let bytes = serde_json::to_vec(body).expect("body serializes");
    let fields: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    arrive("POST", target, fields, &bytes, &()).expect("the request arrives")
}

/// RED: the reader's refusal of the unknown role made the projection fail, and the door refused
#[test]
fn an_unknown_role_is_screened_under_its_own_role_and_every_turn_is_kept() {
    let body = serde_json::json!({
        "model": "m0",
        "messages": [
            {"role": "wizard", "content": "hi"},
            {"role": "user", "content": "READABLE"},
            {"content": "no role"}
        ]
    });
    let view = project(&arrived("/v1/chat/completions", &body));
    assert_eq!(
        view.turns,
        vec![
            ("wizard".to_string(), "hi".to_string()),
            ("user".to_string(), "READABLE".to_string()),
            (String::new(), "no role".to_string()),
        ],
        "every turn kept, under the role the body states"
    );
    assert_eq!(view.turn_count, 3);
    assert_eq!(
        view.text_chars,
        "hi".len() + "READABLE".len() + "no role".len()
    );
    assert_eq!(view.system, None);
    assert!(!view.has_tools);
}

/// The raw view keeps 1.5.5's shapes for a refused body: a block-array system prompt flattened, a
#[test]
fn a_refused_body_keeps_the_system_prompt_blocks_tools_and_end_user() {
    let body = serde_json::json!({
        "model": "m0",
        "system": [{"type": "text", "text": "SYS"}],
        "user": "alice",
        "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
        "max_tokens": 64,
        "messages": [
            {"role": "wizard", "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}},
                {"type": "text", "text": "two"}
            ]}
        ]
    });
    let view = project(&arrived("/v1/messages", &body));
    assert_eq!(view.system.as_deref(), Some("SYS"));
    assert_eq!(
        view.turns,
        vec![("wizard".to_string(), "one\ntwo".to_string())]
    );
    assert_eq!(view.text_chars, "SYS".len() + "one".len() + "two".len());
    assert!(view.has_tools);
    assert_eq!(view.end_user.as_deref(), Some("alice"));
    assert_eq!(view.max_tokens, Some(64));
}
