// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! ABSENT SEMANTICS for the hook projection, on the plane that projects it (ported from the

use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::project::{project, HookView};

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
