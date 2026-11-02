// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/handlers/anthropic.rs`.

use super::*;

/// Anthropic serves chat only: `operation_handler` must answer `Some` for `Chat` and `None`

#[test]
fn operation_handler_serves_chat_only() {
    let h = AnthropicRequestHandler;
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
            "{op:?} must have no handler on the Anthropic protocol"
        );
    }
}

/// A request path ending in `/v1/messages` resolves to `Chat`; anything else resolves to

#[test]
fn resolve_operation_matches_the_messages_path_only() {
    let h = AnthropicRequestHandler;
    assert_eq!(h.resolve_operation("/v1/messages", b""), Some(OpVerb::CHAT));

    assert_eq!(
        h.resolve_operation("/proxy/upstream/v1/messages", b""),
        Some(OpVerb::CHAT)
    );

    for path in [
        "/v1/chat/completions",
        "/v1/complete",
        "/v1/messages/extra",
        "/v1/message",
        "/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude:rawPredict",
        "",
    ] {
        assert_eq!(
            h.resolve_operation(path, b""),
            None,
            "path '{path}' must not resolve to any operation"
        );
    }
}

#[test]
fn path_base_uses_vertex_rawpredict_with_model_in_url() {
    let h = AnthropicRequestHandler;
    let model = "claude-3-5-sonnet";
    let ctx = |stream, path_base| EgressCtx {
        operation: OpVerb::CHAT,
        model,
        stream,
        path_base,
    };

    assert_eq!(h.upstream_path(&ctx(false, None)), "/v1/messages");

    let vbase = "/v1/projects/p/locations/us-central1/publishers/anthropic/models";
    assert_eq!(
            h.upstream_path(&ctx(false, Some(vbase))),
            "/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude-3-5-sonnet:rawPredict"
        );

    assert_eq!(
            h.upstream_path(&ctx(true, Some(vbase))),
            "/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude-3-5-sonnet:streamRawPredict"
        );
}
