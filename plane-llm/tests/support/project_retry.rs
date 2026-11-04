// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use busbar_contract::abi::hook::MessageView;
use busbar_contract::abi::mechanism::{
    call::InHead,
    lifecycle::{slot, OpenIn, OpenOut},
};
use busbar_contract::abi::sdk::door::Entry;

slot!(TestOpen, OpenIn, OpenOut, |instance, input, _out| {
    let target = if input.settings().bytes() == b"anthropic" {
        "/v1/messages"
    } else {
        "/v1/chat/completions"
    };
    let shaping = Arc::new(Shaping::from_settings(&serde_json::json!({})).unwrap());
    let mut unit = UnitState::new(shaping);
    unit.arrived = Some(
        arrive::arrive(
            "POST",
            target,
            &[(b"content-type", b"application/json")],
            br#"{"model":"m","messages":[{"role":"user","content":"original"}]}"#,
            &(),
        )
        .unwrap(),
    );
    let mut book = UnitBook::default();
    book.hold(1, unit);
    instance.open(LlmDoor {
        generations: Generations::new(),
        shapings: Mutex::new(BTreeMap::new()),
        units: Mutex::new(book),
        services: None,
        webhooks: Webhooks::default(),
    });
    Outcome::Ready
});
slot!(TestClose, InHead, OutHead, |_, _, _| { Outcome::Ready });

fn retry(short_arena: bool, dialect: &[u8]) {
    // SAFETY: these ABI records contain only integers, raw pointers and zero-valid ABI wrappers.
    let (mut open, mut opened): (OpenIn, OpenOut) = unsafe { std::mem::zeroed() };
    // SAFETY: dialect remains live across open.
    open.settings = unsafe { Blob::from_raw_parts(dialect.as_ptr(), dialect.len(), 0, 0) };
    // SAFETY: OPEN receives no instance and all buffers are valid for the call.
    assert_eq!(
        unsafe { Safe::<TestOpen>::enter(std::ptr::null_mut(), slot::OPEN, &open, &mut opened) },
        Outcome::Ready
    );
    let instance = opened.instance;
    let rewrite = br#"{"messages":[{"role":"user","content":"rewritten"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]}"#;
    let mut arena = vec![0u8; 8192];
    // SAFETY: MessageView consists of ABI strings and zero-valid scalar fields.
    let mut messages: [MessageView; 4] = unsafe { std::mem::zeroed() };
    // SAFETY: ProjectIn is a zero-valid ABI record; referenced buffers are installed below.
    let mut input: ProjectIn = unsafe { std::mem::zeroed() };
    input.unit = 1;
    input.arena_buf = arena.as_mut_ptr();
    input.arena_cap = if short_arena { 0 } else { arena.len() };
    input.messages_buf = messages.as_mut_ptr();
    input.messages_cap = if short_arena { messages.len() } else { 0 };
    // SAFETY: rewrite remains live across every call below.
    input.rewrite = unsafe { Blob::from_raw_parts(rewrite.as_ptr(), rewrite.len(), 0, 0) };
    // SAFETY: output is a zero-valid ABI record.
    let mut out: ProjectOut = unsafe { std::mem::zeroed() };
    // SAFETY: SDK-minted instance and live input buffers, no overlapping calls.
    assert_eq!(
        unsafe {
            Safe::<Project>::enter(
                instance,
                busbar_contract::abi::plane::slot::PROJECT,
                &input,
                &mut out,
            )
        },
        Outcome::Failed
    );
    assert!(out.arena_needed > 0);
    assert_eq!(out.messages_needed, 1);
    input.arena_cap = arena.len();
    input.messages_cap = messages.len();
    for expected_tools in [1, 2] {
        // SAFETY: the same live instance and buffers; the second success is a distinct hook.
        assert_eq!(
            unsafe {
                Safe::<Project>::enter(
                    instance,
                    busbar_contract::abi::plane::slot::PROJECT,
                    &input,
                    &mut out,
                )
            },
            Outcome::Ready
        );
        let span = out.rewritten;
        let body: Value =
            serde_json::from_slice(&arena[span.offset as usize..(span.offset + span.len) as usize])
                .unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), expected_tools);
        assert_eq!(body["messages"][0]["content"], "rewritten");
    }
    // SAFETY: close the SDK-minted instance once, with zero-valid records and no outstanding call.
    unsafe {
        let head: InHead = std::mem::zeroed();
        let mut closed: OutHead = std::mem::zeroed();
        assert_eq!(
            Safe::<TestClose>::enter(instance, slot::CLOSE, &head, &mut closed),
            Outcome::Ready
        );
    }
}

#[test]
fn project_arena_retry_commits_hook_tools_once() {
    for dialect in [b"openai".as_slice(), b"anthropic".as_slice()] {
        retry(true, dialect);
    }
}

#[test]
fn project_messages_retry_commits_hook_tools_once() {
    for dialect in [b"openai".as_slice(), b"anthropic".as_slice()] {
        retry(false, dialect);
    }
}
