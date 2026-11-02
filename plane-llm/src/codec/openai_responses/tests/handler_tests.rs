// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/handlers/responses.rs`.

use super::*;

/// The Responses API serves chat only: `operation_handler` must answer `Some` for `Chat` and
#[test]
fn operation_handler_serves_chat_only() {
    let h = ResponsesRequestHandler;
    assert!(h.operation_handler(OpVerb::CHAT).is_some());
    for op in [
        OpVerb::EMBEDDINGS,
        OpVerb::MODERATION,
        OpVerb::IMAGE,
        OpVerb::TRANSCRIPTION,
        OpVerb::SPEECH,
        OpVerb::RERANK,
    ] {
        assert!(
            h.operation_handler(op).is_none(),
            "{op:?} must have no handler on the Responses protocol"
        );
    }
}

/// `upstream_path` must egress to the REAL `/v1/responses` endpoint — a path-string typo here
#[test]
fn upstream_path_is_the_real_responses_endpoint() {
    let h = ResponsesRequestHandler;
    let ctx = EgressCtx {
        operation: OpVerb::CHAT,
        model: "gpt-4.1",
        stream: false,
        path_base: None,
    };
    assert_eq!(h.upstream_path(&ctx), "/v1/responses");

    let ctx_streaming = EgressCtx {
        stream: true,
        ..ctx
    };
    assert_eq!(h.upstream_path(&ctx_streaming), "/v1/responses");
}

/// A request path ending in `/v1/responses` resolves to `Chat`; anything else resolves to
#[test]
fn resolve_operation_matches_the_responses_path_only() {
    let h = ResponsesRequestHandler;
    assert_eq!(
        h.resolve_operation("/v1/responses", b""),
        Some(OpVerb::CHAT)
    );

    assert_eq!(
        h.resolve_operation("/proxy/upstream/v1/responses", b""),
        Some(OpVerb::CHAT)
    );

    for path in [
        "/v1/chat/completions",
        "/v1/embeddings",
        "/v1/responses/extra",
        "/v1/response",
        "",
    ] {
        assert_eq!(
            h.resolve_operation(path, b""),
            None,
            "path '{path}' must not resolve to any operation"
        );
    }
}
