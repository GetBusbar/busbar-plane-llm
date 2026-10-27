//! CONFORMANCE (Anthropic's published `ErrorResponse`): an in-stream `error` event's `error.type` is
//! one of nine tokens, and `error.message` is the human-readable text. v1.5.5 put the provider's
//! signal in `type` verbatim, so free text ("The response stream was interrupted.") landed in the
//! discriminator, which no SDK maps to an exception.
use super::super::proto_codec::protocol_for;
use crate::codec::ir::IrStreamEvent;
use busbar_contract::protocol::IrError;
use busbar_contract::upstream::StatusClass;

const TOKENS: [&str; 9] = [
    "api_error",
    "authentication_error",
    "billing_error",
    "invalid_request_error",
    "not_found_error",
    "overloaded_error",
    "permission_error",
    "rate_limit_error",
    "timeout_error",
];

fn error_event(class: StatusClass, signal: Option<&str>) -> serde_json::Value {
    let ev = IrStreamEvent::Error(IrError {
        class,
        provider_signal: signal.map(str::to_string),
        retry_after: None,
    });
    let (name, data) = protocol_for("anthropic")
        .unwrap()
        .writer()
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("an error event is written");
    assert_eq!(name, "error");
    data
}

#[test]
fn free_text_signal_is_the_message_and_the_type_is_the_classs_token() {
    let d = error_event(
        StatusClass::ServerError,
        Some("The response stream was interrupted."),
    );
    assert_eq!(d["error"]["type"], "api_error", "{d}");
    assert_eq!(
        d["error"]["message"], "The response stream was interrupted.",
        "{d}"
    );
}

#[test]
fn every_class_yields_a_published_type_token() {
    for class in StatusClass::ALL {
        let d = error_event(*class, Some("some provider text"));
        let t = d["error"]["type"].as_str().expect("type is a string");
        assert!(TOKENS.contains(&t), "{class:?} -> {t:?}");
        assert_eq!(d["error"]["message"], "some provider text");
    }
}

#[test]
fn a_signal_that_is_already_a_type_token_stays_the_type() {
    let d = error_event(StatusClass::Overloaded, Some("overloaded_error"));
    assert_eq!(d["error"]["type"], "overloaded_error", "{d}");
}

#[test]
fn no_signal_gets_the_class_token_and_a_generic_message() {
    let d = error_event(StatusClass::RateLimit, None);
    assert_eq!(d["error"]["type"], "rate_limit_error", "{d}");
    assert!(!d["error"]["message"].as_str().unwrap().is_empty());
}
