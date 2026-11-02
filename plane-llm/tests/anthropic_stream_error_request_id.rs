// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! An Anthropic in-stream `error` event is Anthropic's ErrorResponse, and the published schema

mod common;

use busbar_contract::upstream::{CanonicalSignal, StatusClass};
use busbar_plane_llm::codec::anthropic;
use busbar_plane_llm::codec::ir::IrStreamEvent;
use common::SevensHost;

#[test]
fn an_anthropic_in_stream_error_event_carries_every_member_error_response_requires() {
    let writer = anthropic::protocol();
    for (class, signal) in [
        (StatusClass::Overloaded, Some("overloaded_error")),
        (StatusClass::RateLimit, None),
        (StatusClass::ServerError, Some("a sentence, not a token")),
    ] {
        let err = CanonicalSignal {
            class,
            provider_signal: signal.map(str::to_string),
            retry_after: None,
        };
        let (event, data) = writer
            .writer()
            .write_response_event(&SevensHost, &IrStreamEvent::Error(err))
            .expect("an error event is written");
        assert_eq!(event, "error");
        let obj = data.as_object().expect("the event body is an object");
        for member in ["type", "error", "request_id"] {
            assert!(
                obj.contains_key(member),
                "{class:?}: ErrorResponse requires `{member}`; got {data}"
            );
        }
        let rid = obj["request_id"].as_str().expect("a minted request id");
        assert!(rid.starts_with("req_") && rid.len() > "req_".len(), "{rid}");
    }
}
