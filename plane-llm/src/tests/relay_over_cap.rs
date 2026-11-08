// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Large same-dialect delivered answers retain reported billing with bounded root custody.

use crate::common;

use crate::exchange::reply::relay::{Relay, RelayCtx};
use busbar_contract::operation::OpVerb;
use common::TestHost;

/// The handle every codec call here is handed.
const HOST: &dyn crate::codec::cells::CodecHost = &TestHost;

/// The usage tap's cap these relays carry.
const CAP: usize = 64;

fn embeddings_handler() -> &'static dyn crate::codec::cells::OperationHandler {
    crate::codec::DECLS
        .iter()
        .find(|d| d.name == "openai")
        .and_then(|d| d.handler)
        .and_then(|rh| rh.operation_handler(OpVerb::EMBEDDINGS))
        .expect("the openai dialect serves embeddings")
}

fn relay() -> Relay {
    Relay::new(RelayCtx {
        ingress: "openai",
        egress: "openai",
        far_is_stream: false,
        json_array: false,
        client_include_usage: false,
        request: None,
        handler: embeddings_handler(),
        meter: true,
        translate_cap: CAP,
    })
}

/// An embeddings answer of `pad` floats before its usage (5 tokens).
fn body(pad: usize) -> Vec<u8> {
    let floats = vec!["0.25"; pad].join(",");
    format!(
        "{{\"object\":\"list\",\"data\":[{{\"object\":\"embedding\",\"index\":0,\"embedding\":[{floats}]}}],\"model\":\"m\",\"usage\":{{\"prompt_tokens\":5,\"total_tokens\":5}}}}"
    )
    .into_bytes()
}

/// The large embedding payload must not hide its independently reported usage.
#[test]
fn a_body_past_the_usage_taps_cap_and_one_under_it_bill_reported_usage() {
    let over = body(200);
    assert!(over.len() > CAP);
    let mut r = relay();
    for piece in over.chunks(16) {
        let _ = r.feed(HOST, piece);
    }
    assert!(
        r.incurred_usage().is_none(),
        "nothing incurred past the cap"
    );
    let end = r.end(HOST);
    assert_eq!(end.usage.expect("reported usage above cap").input, 5);

    let under: &[u8] = br#"{"data":[],"usage":{"prompt_tokens":5,"total_tokens":5}}"#;
    assert!(under.len() <= CAP);
    let mut r = relay();
    let _ = r.feed(HOST, under);
    let end = r.end(HOST);
    assert!(end.usage.is_some(), "a body under the cap bills its usage");
}

#[test]
fn capped_usage_is_chunk_independent_and_never_nested_or_fabricated() {
    let reported = body(200);
    for size in [1, 2, 7, 64, reported.len()] {
        let mut r = relay();
        for chunk in reported.chunks(size) {
            let _ = r.feed(HOST, chunk);
        }
        assert_eq!(r.end(HOST).usage.unwrap().input, 5);
    }
    for body in [
        format!(
            r#"{{"data":[{{"usage":{{"prompt_tokens":999}}}}],"padding":"{}"}}"#,
            "x".repeat(200)
        ),
        format!(
            r#"{{"data":"{}","usage":{{"prompt_tokens":5,"padding":"{}"}}}}"#,
            "x".repeat(200),
            "x".repeat(200)
        ),
        format!(
            r#"{{"data":[1,,2],"padding":"{}","usage":{{"prompt_tokens":5}}}}"#,
            "x".repeat(200)
        ),
    ] {
        let mut r = relay();
        for chunk in body.as_bytes().chunks(3) {
            let _ = r.feed(HOST, chunk);
        }
        assert!(r.end(HOST).usage.is_none(), "{body}");
    }

    // The later null replaces the earlier report. The leaf tap retains a present usage
    // envelope as an authoritative empty count carrier, with no reported billing classes.
    let body = format!(
        r#"{{"data":"{}","usage":{{"prompt_tokens":5}},"usage":null}}"#,
        "x".repeat(200)
    );
    let mut r = relay();
    for chunk in body.as_bytes().chunks(3) {
        let _ = r.feed(HOST, chunk);
    }
    let end = r.end(HOST);
    let empty = busbar_contract::billing::TokenUsage {
        exact_counts: Some(Default::default()),
        ..Default::default()
    };
    assert_eq!(end.usage, Some(empty));
    assert!(end.open_units.is_empty());
    assert_eq!(
        crate::exchange::reply::Units::of(end.usage.as_ref(), end.open_units),
        crate::exchange::reply::Units {
            exact_counts: Some(Default::default()),
            ..Default::default()
        },
        "no billing rows, refused counts, open units, tokens, or floor"
    );
}

#[test]
fn stream_media_types_match_the_complete_case_insensitive_token() {
    use crate::exchange::reply::wire::is_stream_content_type;
    for ct in [
        "Text/Event-Stream",
        " text/event-stream ; charset=utf-8",
        "APPLICATION/VND.AMAZON.EVENTSTREAM",
    ] {
        assert!(is_stream_content_type(ct), "{ct}");
    }
    for ct in [
        "text/event-streamgarbage",
        "application/vnd.amazon.eventstream-extra",
        "application/json",
    ] {
        assert!(!is_stream_content_type(ct), "{ct}");
    }
}

fn operation_relay(protocol: &str, verb: OpVerb) -> Relay {
    let handler = crate::codec::DECLS
        .iter()
        .find(|d| d.name == protocol)
        .and_then(|d| d.handler)
        .and_then(|rh| rh.operation_handler(verb))
        .expect("operation handler");
    Relay::new_for_operation(
        RelayCtx {
            ingress: protocol,
            egress: protocol,
            far_is_stream: false,
            json_array: false,
            client_include_usage: false,
            request: None,
            handler,
            meter: true,
            translate_cap: CAP,
        },
        verb,
    )
}

#[test]
fn capped_rerank_keeps_reported_open_billing() {
    let body = format!(
        r#"{{"results":[],"padding":"{}","meta":{{"billed_units":{{"search_units":2}}}}}}"#,
        "x".repeat(200)
    );
    let mut relay = operation_relay("cohere", OpVerb::RERANK);
    for chunk in body.as_bytes().chunks(1) {
        let _ = relay.feed(HOST, chunk);
    }
    let usage = relay.end(HOST).usage.expect("reported rerank billing");
    assert_eq!(usage.open_units.get("search_units"), Some(&2));
    assert_eq!((usage.input, usage.output), (0, 0));
}

#[test]
fn capped_provider_failure_does_not_bill_projected_usage() {
    let body = format!(
        r#"{{"status":"failed","error":{{"code":"server_error"}},"output":[],"padding":"{}","usage":{{"input_tokens":5,"output_tokens":2}}}}"#,
        "x".repeat(200)
    );
    let mut relay = operation_relay("responses", OpVerb::CHAT);
    for chunk in body.as_bytes().chunks(2) {
        let _ = relay.feed(HOST, chunk);
    }
    assert!(relay.end(HOST).usage.is_none());
}
