// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE USAGE-TAP FAULT PIN: every way a same-protocol
//! usage tap can fail to read a 2xx body is RETURNED as its fault, under the reason that says which
//! way, whether the tap is the chat cell's own (unknown protocol / bad JSON / a body its reader
//! refuses) or a leaf cell's default (a decode failure).

use crate::codec::cells::{OperationHandler, UsageTapFault};
use busbar_contract::operation::OpVerb;

fn cell(op: OpVerb) -> &'static dyn OperationHandler {
    crate::codec::bedrock::DECL
        .handler
        .and_then(|h| h.operation_handler(op))
        .expect("the signing dialect serves the operation")
}

/// A JSON body the signing dialect's chat reader refuses (it is not a Converse document).
const UNREADABLE: &[u8] = b"[1,2,3]";
const NOT_JSON: &[u8] = b"{not json";

#[test]
fn the_chat_cells_three_fault_paths_each_return_their_reason() {
    let chat = crate::codec::chat_handle::ChatOperation("bedrock");
    let unknown = crate::codec::chat_handle::ChatOperation("no-such-dialect");
    let faults = [
        chat.extract_usage(crate::codec::test_host::HOST, "bedrock", UNREADABLE),
        chat.extract_usage(crate::codec::test_host::HOST, "bedrock", NOT_JSON),
        unknown.extract_usage(crate::codec::test_host::HOST, "no-such-dialect", b"{}"),
    ];
    assert_eq!(
        faults,
        [
            Err(UsageTapFault::Decode),
            Err(UsageTapFault::BadJson),
            Err(UsageTapFault::UnknownProtocol),
        ]
    );
    let reasons: Vec<&str> = faults
        .iter()
        .filter_map(|f| f.as_ref().err().map(|f| f.reason()))
        .collect();
    assert_eq!(reasons, ["decode", "bad_json", "unknown_protocol"]);
}

#[test]
fn a_leaf_cells_default_tap_returns_a_refused_body_as_a_decode_fault() {
    for op in [OpVerb::EMBEDDINGS, OpVerb::IMAGE] {
        assert_eq!(
            cell(op).extract_usage(crate::codec::test_host::HOST, "bedrock", NOT_JSON),
            Err(UsageTapFault::Decode),
            "{op:?}"
        );
    }
}

/// A body the tap reads is no fault, whatever usage it states.
#[test]
fn a_readable_body_is_no_fault() {
    let chat = crate::codec::chat_handle::ChatOperation("bedrock");
    let body = br#"{"output":{"message":{"role":"assistant","content":[{"text":"hi"}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5}}"#;
    assert!(chat
        .extract_usage(crate::codec::test_host::HOST, "bedrock", body)
        .is_ok());
}

fn assert_exact_numeric_body(raw: &str, leaf: bool, refused: bool) {
    let body = if leaf {
        format!(r#"{{"embedding":[0.1],"inputTextTokenCount":{raw}}}"#)
    } else {
        format!(
            r#"{{"output":{{"message":{{"role":"assistant","content":[{{"text":"hi"}}]}}}},"stopReason":"end_turn","usage":{{"inputTokens":{raw},"outputTokens":0}}}}"#
        )
    };
    let usage = cell(if leaf {
        OpVerb::EMBEDDINGS
    } else {
        OpVerb::CHAT
    })
    .extract_usage(crate::codec::test_host::HOST, "bedrock", body.as_bytes())
    .unwrap()
    .unwrap();
    let units = crate::exchange::reply::Units::of(Some(&usage), Default::default());
    assert_eq!(
        !units.refused_counts.is_empty(),
        refused,
        "{raw}, leaf={leaf}"
    );
    assert!(!units.floor);
    let reported = crate::plane_door::counts(&units);
    let input = reported.iter().find(|u| u.class == 0);
    if refused {
        assert!(input.is_none());
    } else {
        assert_eq!(
            input.unwrap().count().unwrap(),
            busbar_contract::Count::parse(raw).unwrap()
        );
    }
}

#[test]
fn exact_numeric_ingress_whole_preserves_microcounts() {
    for raw in ["9007199254740.000001", "9223372036854.775807"] {
        assert_exact_numeric_body(raw, false, false);
    }
}

#[test]
fn exact_numeric_ingress_whole_refuses_unrepresentable_decimals() {
    for raw in ["27.0000000000000001", "9223372036854.775808"] {
        assert_exact_numeric_body(raw, false, true);
    }
}

#[test]
fn exact_numeric_ingress_leaf_preserves_microcounts() {
    for raw in ["9007199254740.000001", "9223372036854.775807"] {
        assert_exact_numeric_body(raw, true, false);
    }
}

#[test]
fn exact_numeric_ingress_leaf_refuses_unrepresentable_decimals() {
    for raw in ["27.0000000000000001", "9223372036854.775808"] {
        assert_exact_numeric_body(raw, true, true);
    }
}
