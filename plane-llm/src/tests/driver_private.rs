// SPDX-License-Identifier: Apache-2.0
//! Private assertions split out of the assembled driver fixtures.

use crate::dialect::DIALECTS;
use crate::refusal::REFUSAL_STATUSES;
use busbar_contract::caps::ReasonCode;
use busbar_kernel::plane_driver::{refusal_status, BufferCaps, DriverConfig};
use serde_json::{json, Value};

fn config() -> DriverConfig {
    DriverConfig {
        caps: BufferCaps::default(),
        op_classes: Vec::new(),
        status_of: refusal_status,
        refusal_statuses: REFUSAL_STATUSES.to_vec(),
        caller_refs: None,
    }
}

fn dialect(name: &str) -> u32 {
    u32::try_from(
        DIALECTS
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("the plane states {name}")),
    )
    .expect("a dialect index")
}

const SIX: [&str; 6] = [
    "anthropic",
    "openai",
    "gemini",
    "bedrock",
    "responses",
    "cohere",
];

/// LEGACY `crates/busbar-llm/tests/llm_refusal_statuses.rs::p_item_refusal_reason_collapse_no_llm_refusal_reads_as_an_internal_error`
#[test]
fn p_item_refusal_reason_collapse_no_llm_refusal_reads_as_an_internal_error() {
    use crate::exchange::refuse::kind_of;
    use busbar_contract::abi::plane::{class_of, RefusalClass};
    let config = config();
    let mut wrong = Vec::new();
    for d in SIX {
        for reason in ReasonCode::ALL {
            let class = class_of(*reason);
            let status = config.status(dialect(d), *reason);
            let kind = kind_of(reason.as_str(), u16::try_from(status).expect("a status"));
            if status == 500 {
                wrong.push(format!("{d} {reason:?} answers a bare 500"));
            }
            let generic = kind == crate::codec::protocol::KIND_API_ERROR;
            if generic != (class.is_node_fault() || class == RefusalClass::Timeout) {
                wrong.push(format!(
                    "{d} {reason:?} ({class:?}) reads as {kind} at {status}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
/// The chat-completions caller's own spelling of busbar's overloaded 503 kind, read off the plane's
/// own refusal renderer, so the test pins "the same kind as every other busbar 503" and no literal.
fn overloaded_kind_on_chat_completions() -> String {
    struct NoHost;
    impl crate::codec::cells::CodecHost for NoHost {
        fn fill_entropy(&self, _out: &mut [u8]) -> bool {
            false
        }
        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }
    let rendered = crate::exchange::refuse::render(
        &NoHost,
        "openai",
        503,
        crate::codec::protocol::KIND_OVERLOADED,
        "x",
        0,
    );
    let v: Value = serde_json::from_slice(&rendered.body).expect("a refusal is JSON");
    v["error"]["type"].as_str().unwrap_or_default().to_string()
}

#[test]
fn chat_overload_wire_kind_is_server_error() {
    assert_eq!(overloaded_kind_on_chat_completions(), "server_error");
}

#[test]
fn empty_and_non_chat_rewrites_leave_arrivals_unchanged() {
    use crate::exchange::arrive::arrive;
    use crate::exchange::project::apply_rewrite;
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let rewrite = br#"{"messages":[{"role":"user","content":"compressed"}],"tools":[]}"#;
    let non_chat = br#"{"model":"m0","contents":[{"parts":[]}]}"#;
    if let Ok(mut arrived) = arrive("POST", "/v1/chat/completions", head, non_chat, &()) {
        let before = arrived.body.clone();
        assert_eq!(
            apply_rewrite(&mut arrived, rewrite),
            None,
            "non-chat body left untouched (fail-safe)"
        );
        assert_eq!(arrived.body, before);
    }
    let chat = br#"{"model":"m0","messages":[{"role":"user","content":"x"}]}"#;
    let mut arrived = arrive("POST", "/v1/chat/completions", head, chat, &()).expect("arrives");
    let before = arrived.body.clone();
    assert_eq!(
        apply_rewrite(&mut arrived, br#"{"messages":[],"tools":[]}"#),
        None,
        "an empty rewrite changes nothing"
    );
    assert_eq!(arrived.body, before);
}
/// The plane's own rewrite step over `body` arriving at `target`: the rewritten body, `None` when
/// the rewrite leaves the arrival untouched (and then the arrival's body is unchanged).
fn applied(target: &str, body: &[u8], rewrite: &Value) -> Option<Value> {
    use crate::exchange::arrive::arrive;
    use crate::exchange::project::apply_rewrite;
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let mut arrived = arrive("POST", target, head, body, &()).expect("the body arrives");
    let before = arrived.body.clone();
    let rewrite = serde_json::to_vec(rewrite).expect("serializes");
    match apply_rewrite(&mut arrived, &rewrite) {
        Some(bytes) => Some(serde_json::from_slice(&bytes).expect("the rewritten body is JSON")),
        None => {
            assert_eq!(
                arrived.body, before,
                "an unapplied rewrite leaves the body untouched"
            );
            None
        }
    }
}

#[test]
fn responses_and_bedrock_rewrite_preserve_value_semantics() {
    let rewrite = vec![
        json!({"role":"user","content":"compressed"}),
        json!({"role":"assistant","content":"prior"}),
    ];
    let r = applied(
        "/v1/responses",
        br#"{"model":"m0","input":"orig"}"#,
        &json!({"messages": rewrite, "tools": []}),
    )
    .expect("the responses rewrite applies");
    assert_eq!(r["input"][0]["content"], "compressed");

    let blocky =
        json!({"messages": [{"role": "user", "content": [{"type": "text"}]}], "tools": []});
    assert_eq!(
        applied(
            "/model/m0/converse",
            br#"{"messages":[{"role":"user","content":[{"text":"orig"}]}]}"#,
            &blocky,
        ),
        None,
        "a non-text rewrite leaves bedrock untouched"
    );
}
