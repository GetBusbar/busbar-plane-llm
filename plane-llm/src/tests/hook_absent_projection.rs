// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! ABSENT SEMANTICS for the hook projection, on the plane that projects it (ported from the
//! predev-only `busbar-llm` test `hook_non_chat_projection_tests::
//! absent_semantics_hold_for_opless_and_bodyless`, whose `read_hook_facts` is the plane's `project`
//! on the driver): an arrival the plane has no operation reader for, or one with no body, projects
//! the EMPTY view (no turn, no system text, no size signal).

use crate::exchange::arrive::Arrived;
use crate::exchange::project::{project, HookView};

fn arrived(dialect: &'static str, body: &[u8], parsed: Option<serde_json::Value>) -> Arrived {
    Arrived {
        dialect,
        operation: busbar_contract::operation::OpVerb::EMBEDDINGS,
        model: "m0".to_string(),
        content_type: "application/json".to_string(),
        body: body.to_vec(),
        parsed,
        path: "/v1/embeddings".to_string(),
        query: None,
        path_model: None,
    }
}

#[test]
fn absent_semantics_hold_for_opless_and_bodyless() {
    let v = serde_json::json!({"input": "x"});
    let view = project(&arrived(
        "not-a-protocol",
        &serde_json::to_vec(&v).expect("serializes"),
        Some(v),
    ));
    assert_eq!(view, HookView::default());

    let view = project(&arrived("openai", b"", None));
    assert_eq!(view.turns, Vec::<(String, String)>::new());
    assert_eq!(
        (view.system, view.text_chars, view.turn_count),
        (None, 0, 0)
    );
}

fn chat_view(dialect: &'static str, value: serde_json::Value) -> HookView {
    let mut request = arrived(
        dialect,
        &serde_json::to_vec(&value).unwrap(),
        Some(value.clone()),
    );
    request.operation = busbar_contract::operation::OpVerb::CHAT;
    project(&request)
}

#[test]
fn complete_projection_requires_all_normalized_prompt_content_to_be_text() {
    use serde_json::json;
    let plain = json!({"model":"m0","messages":[{"role":"system","content":"system"},{"role":"user","content":"hello"}]});
    assert!(chat_view("openai", plain.clone()).text_complete);
    for content in [
        json!([{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"https://example.test/image.png"}}]),
        json!([{"type":"text","text":"hello"},{"type":"input_audio","input_audio":{"data":"AA==","format":"wav"}}]),
        json!([{"type":"text","text":"hello"},{"type":"future_unmodelled_content","payload":"opaque"}]),
    ] {
        let mut value = plain.clone();
        value["messages"][1]["content"] = content;
        assert!(!chat_view("openai", value).text_complete);
    }
    let mut tools = plain.clone();
    tools["tools"] =
        json!([{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]);
    assert!(!chat_view("openai", tools).text_complete);
    let mut history = plain;
    history["messages"][1] = json!({"role":"tool","tool_call_id":"a","content":"result"});
    assert!(!chat_view("openai", history).text_complete);
}

#[test]
fn complete_projection_never_uses_raw_fallback_or_remote_history() {
    use serde_json::json;
    assert!(!chat_view("openai", json!({"messages":"malformed"})).text_complete);
    assert!(
        !chat_view(
            "responses",
            json!({"model":"m0","input":"continue","previous_response_id":"stored-response"})
        )
        .text_complete
    );
}
