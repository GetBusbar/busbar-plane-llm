// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar-core/src/proto/openai_family.rs`.

use busbar_contract::http::StatusCode;
use busbar_contract::upstream::StatusClass;

#[test]
fn test_openai_classify() {
    let protocol = crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
    let reader = protocol.reader();

    let signal = reader.classify(StatusCode::TOO_MANY_REQUESTS, b"{}");
    assert_eq!(signal.class, StatusClass::RateLimit);

    let signal = reader.classify(StatusCode::UNAUTHORIZED, b"{}");
    assert_eq!(signal.class, StatusClass::Auth);

    let signal = reader.classify(StatusCode::SERVICE_UNAVAILABLE, b"{}");
    assert_eq!(signal.class, StatusClass::ServerError);

    let signal = reader.classify(StatusCode::FORBIDDEN, b"{}");
    assert_eq!(signal.class, StatusClass::Auth);
}

/// The classifier must run the SHARED prose scan, not a clause of its own.
#[test]
fn every_context_length_phrasing_the_shared_scan_knows_classifies_as_context_length() {
    let protocol = crate::codec::proto_codec::protocol_for("openai").expect("openai should exist");
    let reader = protocol.reader();
    for prose in [
        "This model's maximum context length is 8192 tokens",
        "Context length exceeded for this request",
        "Please reduce the length of the messages",
        "Your input exceeds the context window",
    ] {
        let body = format!(r#"{{"error":{{"message":"{prose}"}}}}"#);
        assert_eq!(
            reader
                .classify(StatusCode::BAD_REQUEST, body.as_bytes())
                .class,
            StatusClass::ContextLength,
            "an oversized request saying `{prose}` must fail over rather than penalize the lane"
        );
    }

    assert_eq!(
        reader
            .classify(
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"error":{"message":"Please reduce the length of the messages"}}"#
            )
            .class,
        StatusClass::RateLimit,
    );
}

/// The shared context-length prose scan must fire on the canonical self-contained phrases and on
/// the `exceeds`+`context`/`token limit` pairing — this is what lets a genuine oversized-request
/// body fail over (to a larger-context model) instead of penalizing the lane's breaker.
#[test]
fn context_length_prose_scan_fires_on_canonical_phrases() {
    use super::context_length_prose_scan as scan;

    assert!(scan("this model's maximum context length is 8192 tokens"));
    assert!(scan("context length exceeded for this request"));
    assert!(scan("please reduce the length of the messages"));

    assert!(scan("your input exceeds the context window"));
    assert!(scan("the request exceeds the token limit"));
}

/// Precision guard: the scan must NOT fire on unrelated errors whose prose merely mentions weak
/// tokens like `token`/`maximum` — e.g. a per-day quota (rate-limit) body — or a bare `exceeds`
/// with no context/token-limit co-location.
#[test]
fn context_length_prose_scan_precise_no_false_positive() {
    use super::context_length_prose_scan as scan;
    assert!(
        !scan("you have reached the maximum number of tokens allowed per day"),
        "a per-day quota body is a rate-limit, not context-length"
    );
    assert!(
        !scan("the request exceeds your monthly spend limit"),
        "`exceeds` without context/token-limit co-location must not fire"
    );
    assert!(!scan("invalid api key provided"));
    assert!(!scan(""));
}

/// `bearer_error_code` mirrors the native OpenAI `type`→`code` pairing: only auth and
/// insufficient-quota carry a machine-readable `code` (the value official SDKs surface as
/// `error.code`); every other modeled type — and any passthrough type — stays `null`, matching
/// the shape OpenAI uses when no code applies.
#[test]
fn bearer_error_code_mirrors_native_type_code_pairing() {
    use super::bearer_error_code as code;
    assert_eq!(
        code(busbar_contract::protocol::KIND_AUTHENTICATION),
        serde_json::Value::String("invalid_api_key".to_string())
    );
    assert_eq!(
        code(busbar_contract::protocol::KIND_INSUFFICIENT_QUOTA),
        serde_json::Value::String("insufficient_quota".to_string())
    );

    for t in [
        busbar_contract::protocol::KIND_INVALID_REQUEST,
        busbar_contract::protocol::KIND_PERMISSION,
        busbar_contract::protocol::KIND_NOT_FOUND,
        busbar_contract::protocol::KIND_RATE_LIMIT,
        crate::codec::framework::KIND_SERVER_ERROR,
        busbar_contract::protocol::KIND_API_ERROR,
    ] {
        assert_eq!(code(t), serde_json::Value::Null, "{t} must emit code:null");
    }

    assert_eq!(code("some_future_error_type"), serde_json::Value::Null);
}
