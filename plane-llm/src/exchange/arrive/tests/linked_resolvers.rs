// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use busbar_contract::codec::RequestHandler;
use serde_json::Value;
use std::sync::LazyLock;

use super::{detect as detect_protocol, residual as residual_dialect_for_path};

static FIXTURE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("linked_protocols.json"))
        .expect("the original root linked-protocols fixture is JSON")
});

fn strs(v: &Value) -> Vec<&str> {
    v.as_array()
        .expect("an array")
        .iter()
        .map(|s| s.as_str().expect("a string"))
        .collect()
}

fn headers(v: &Value) -> Vec<(&[u8], &[u8])> {
    v.as_array()
        .expect("header pairs")
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().expect("name").as_bytes(),
                pair[1].as_str().expect("value").as_bytes(),
            )
        })
        .collect()
}

fn request_handler(proto: &str) -> Option<&'static dyn RequestHandler> {
    crate::exchange::decl(proto).and_then(|decl| decl.handler)
}

#[test]
fn resolver_table() {
    for case in FIXTURE["resolver"].as_array().expect("resolver") {
        let path = case[0].as_str().expect("path");
        let got = detect_protocol(path, &headers(&case[1])).and_then(|proto| {
            request_handler(proto)
                .and_then(|rh| rh.resolve_operation(path, b""))
                .map(|op| (proto, op.name()))
        });
        let want = case[2]
            .as_array()
            .map(|w| (w[0].as_str().expect("proto"), w[1].as_str().expect("verb")));
        assert_eq!(got, want, "path {path:?} headers {}", case[1]);
    }
    for case in FIXTURE["body_resolver"].as_array().expect("body_resolver") {
        let (path, body) = (
            case[0].as_str().expect("path"),
            case[1].as_str().expect("body"),
        );
        let proto = detect_protocol(path, &Vec::new()).expect(path);
        assert_eq!(
            proto,
            case[2][0].as_str().expect("proto"),
            "protocol for {path:?}"
        );
        let op = request_handler(proto)
            .and_then(|rh| rh.resolve_operation(path, body.as_bytes()))
            .expect(path);
        assert_eq!(
            op.name(),
            case[2][1].as_str().expect("verb"),
            "operation for {path:?}"
        );
    }
}

#[test]
fn mandatory_header_beats_path_ordering() {
    let case = &FIXTURE["mandatory_header"];
    let p = detect_protocol(case[0].as_str().expect("path"), &headers(&case[1])).unwrap();
    assert_eq!(p, case[2].as_str().expect("proto"));
}

#[test]
fn test_residual_dialect_colon_model_id_is_openai_not_gemini() {
    for case in FIXTURE["residual"].as_array().expect("residual") {
        let path = case[0].as_str().expect("path");
        assert_eq!(
            residual_dialect_for_path(path),
            case[1].as_str(),
            "residual classification of {path:?}"
        );
    }
}

#[test]
fn test_residual_dialect_names_none_rather_than_defaulting_to_openai() {
    let live = &FIXTURE["residual_live"];
    assert_eq!(
        residual_dialect_for_path(live[0].as_str().expect("path")),
        live[1].as_str(),
        "the residual fold must be live, or the `None` assertions below prove nothing"
    );
    for path in strs(&FIXTURE["residual_none"]) {
        assert_eq!(
            residual_dialect_for_path(path),
            None,
            "`{path}` names no dialect — the classifier must say so, not name a default"
        );
    }
}
