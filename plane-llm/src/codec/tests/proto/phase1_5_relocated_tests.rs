// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! PURE-DIALECT unit tests RELOCATED here from `busbar-core`.

use super::*;

use busbar_contract::ir::facts::IrFacts;

/// UUID-v4 shape checker — a private copy of the core auth suite's helper (which stays in core for

fn assert_uuid_v4_shaped(id: &str) {
    let segs: Vec<&str> = id.split('-').collect();
    assert_eq!(
        segs.iter().map(|s| s.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12],
        "x-amzn-requestid must be UUID-v4 shaped (8-4-4-4-12), got '{id}'"
    );
    assert!(
        id.chars()
            .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "UUID must be lowercase hex with dashes only, got '{id}'"
    );

    assert_eq!(
        segs[2].chars().next(),
        Some('4'),
        "UUID version nibble must be 4, got '{id}'"
    );

    assert!(
        matches!(segs[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
        "UUID variant nibble must be 8/9/a/b, got '{id}'"
    );
}

#[test]
fn test_synth_amzn_request_id_is_uuid_v4() {
    let a = bedrock::synth_amzn_request_id(crate::codec::test_host::HOST)
        .expect("entropy must be available under test");
    let b = bedrock::synth_amzn_request_id(crate::codec::test_host::HOST)
        .expect("entropy must be available under test");
    assert_uuid_v4_shaped(&a);
    assert_uuid_v4_shaped(&b);
    assert_ne!(a, b, "consecutive synthetic request ids must differ");
}

/// The reader's own accept/skip rules for a `reasoning` item's `encrypted_content`: an empty string

#[test]
fn responses_reasoning_reader_rejects_malformed_encrypted_content() {
    let empty_string: serde_json::Value =
        serde_json::json!({"type": "reasoning", "encrypted_content": ""});
    assert!(openai_responses::read_reasoning_encrypted_content(&empty_string).is_none());

    let non_string: serde_json::Value =
        serde_json::json!({"type": "reasoning", "encrypted_content": 123});
    assert!(openai_responses::read_reasoning_encrypted_content(&non_string).is_none());
}

/// A single text-bearing reasoning part (in EITHER `content[]` or `summary[]` alone) must come

#[test]
fn responses_single_part_reasoning_text_borrows() {
    let content_only = serde_json::json!({
        "type": "reasoning",
        "content": [{"type": "reasoning_text", "text": "one part"}]
    });
    assert!(matches!(
        openai_responses::read_reasoning_text(&content_only),
        std::borrow::Cow::Borrowed(_)
    ));

    let summary_only = serde_json::json!({
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "just a summary"}]
    });
    assert!(matches!(
        openai_responses::read_reasoning_text(&summary_only),
        std::borrow::Cow::Borrowed(_)
    ));
}

/// Two `content[]` parts + one `summary[]` part must still allocate — and concatenate

#[test]
fn responses_multi_part_reasoning_text_concatenates() {
    let item = serde_json::json!({
        "type": "reasoning",
        "content": [
            {"type": "reasoning_text", "text": "first "},
            {"type": "reasoning_text", "text": "second "}
        ],
        "summary": [{"type": "summary_text", "text": "third"}]
    });
    let t = openai_responses::read_reasoning_text(&item);
    assert!(matches!(t, std::borrow::Cow::Owned(_)));
    assert_eq!(t.as_ref(), "first second third");
}

/// The bedrock forward-kind → `x-amzn-errortype`/`__type` mapping, for the kinds the ingress error

#[test]
fn error_kind_to_bedrock_type_covers_ingress_emitted_kinds() {
    use busbar_contract::protocol::{
        KIND_INSUFFICIENT_QUOTA, KIND_INVALID_REQUEST, KIND_PERMISSION, KIND_RATE_LIMIT,
    };
    assert_eq!(
        bedrock::error_kind_to_bedrock_type(KIND_INVALID_REQUEST),
        "ValidationException"
    );
    assert_eq!(
        bedrock::error_kind_to_bedrock_type(KIND_RATE_LIMIT),
        "ThrottlingException"
    );
    assert_eq!(
        bedrock::error_kind_to_bedrock_type(KIND_PERMISSION),
        "AccessDeniedException"
    );

    assert_eq!(
        bedrock::error_kind_to_bedrock_type(KIND_INSUFFICIENT_QUOTA),
        "ServiceQuotaExceededException"
    );
}

/// Every kind the bedrock writer can be handed answers an exception that Converse AND

#[test]
fn every_bedrock_error_kind_is_a_converse_declared_exception() {
    const CONVERSE_DECLARED: [&str; 9] = [
        "AccessDeniedException",
        "InternalServerException",
        "ModelErrorException",
        "ModelNotReadyException",
        "ModelTimeoutException",
        "ResourceNotFoundException",
        "ServiceUnavailableException",
        "ThrottlingException",
        "ValidationException",
    ];
    use crate::codec::framework::KIND_SERVER_ERROR;
    use crate::codec::framework::KIND_TIMEOUT;
    use busbar_contract::protocol::{
        KIND_API_ERROR, KIND_INSUFFICIENT_QUOTA, KIND_INVALID_REQUEST, KIND_OVERLOADED,
        KIND_PERMISSION, KIND_RATE_LIMIT,
    };
    let kinds = [
        KIND_INVALID_REQUEST,
        KIND_RATE_LIMIT,
        KIND_PERMISSION,
        KIND_TIMEOUT,
        KIND_OVERLOADED,
        KIND_API_ERROR,
        KIND_SERVER_ERROR,
        "not_found",
        "forbidden",
        "unauthorized",
        "service_unavailable",
        "internal_error",
        "a kind nothing names",
    ];
    for kind in kinds {
        let exc = bedrock::error_kind_to_bedrock_type(kind);
        assert!(
            CONVERSE_DECLARED.contains(&exc),
            "kind {kind:?} answers {exc}, which Converse/ConverseStream do not declare"
        );
    }
    for kind in [
        KIND_INSUFFICIENT_QUOTA,
        "quota_exceeded",
        "service_quota_exceeded",
    ] {
        assert_eq!(
            bedrock::error_kind_to_bedrock_type(kind),
            "ServiceQuotaExceededException",
            "{kind}: the owner's one exception, non-retryable"
        );
    }
}

/// THE EXHAUSTIVENESS GUARD: every registered protocol has a reader that produces a readable IR, so

#[test]
fn every_known_protocol_has_a_declared_reasoning_wire_shape() {
    let protos = known_protocols();
    assert_eq!(
        protos.len(),
        6,
        "the exhaustiveness guard must sweep all six codec dialects, got {protos:?}"
    );
    for &proto in protos {
        let p = protocol_for(proto)
            .unwrap_or_else(|| panic!("'{proto}' is in KNOWN_PROTOCOLS but is not registered"));

        let empty: serde_json::Value =
            serde_json::json!({"messages": [], "contents": [], "input": []});
        let ir = p
            .reader()
            .read_request(&empty)
            .unwrap_or_else(|e| panic!("'{proto}' cannot read an empty conversation: {e:?}"));
        let shape = ir.shape();
        assert_eq!(
            shape.turn_count, 0,
            "'{proto}' invented turns for an empty conversation"
        );
        assert_eq!(shape.text_chars, 0);
        assert!(
            crate::codec::ir::project(&ir).is_empty(),
            "'{proto}' projected content for an empty conversation"
        );
    }
}
