// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for the plane's JSON seam (`json.rs`): every parse goes through the depth guard.

use super::*;

#[test]
fn rejects_pathologically_nested_input_without_overflow() {
    let depth = 1_000_000usize;
    let s = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(
        parse::<serde_json::Value>(s.as_bytes()).is_err(),
        "deeply-nested body must be rejected"
    );
    assert!(exceeds_max_depth(s.as_bytes(), MAX_JSON_DEPTH));
}

#[test]
fn accepts_realistic_depth_and_counts_correctly() {
    let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hi [bracket] {brace} in a string is not depth"}]}]}"#;
    assert!(!exceeds_max_depth(body, MAX_JSON_DEPTH));
    assert!(parse::<serde_json::Value>(body).is_ok());
    assert!(!exceeds_max_depth(br#"{"k":"[[[[[[[[[[ {{{{{{ ]]]]]"}"#, 8));
    let at_limit = format!("{}{}", "[".repeat(128), "]".repeat(128));
    assert!(!exceeds_max_depth(at_limit.as_bytes(), MAX_JSON_DEPTH));
    let over = format!("{}{}", "[".repeat(129), "]".repeat(129));
    assert!(exceeds_max_depth(over.as_bytes(), MAX_JSON_DEPTH));
}

#[test]
fn exact_numeric_ingress_preserves_original_decimal_spelling() {
    for raw in [
        "27.0000000000000001",
        "9007199254740.000001",
        "9223372036854.775807",
    ] {
        let body = format!(r#"{{"count":{raw}}}"#);
        for value in [
            parse::<serde_json::Value>(body.as_bytes()).unwrap(),
            parse_str::<serde_json::Value>(&body).unwrap(),
        ] {
            assert_eq!(value["count"].to_string(), raw);
        }
    }
}

#[test]
fn exact_numbers_serialize_as_json_numbers() {
    for raw in [
        "128",
        "0.5",
        "27.0000000000000001",
        "9007199254740993",
        "1e+400",
    ] {
        let body = format!(r#"{{"count":{raw}}}"#);
        let value = parse::<serde_json::Value>(body.as_bytes()).unwrap();
        assert_eq!(to_vec(&value).unwrap(), body.as_bytes());
        assert_eq!(to_string(&value).unwrap(), body);
    }
}

#[test]
fn literal_number_marker_objects_keep_their_json_type() {
    for key in [
        "$serde_json::private::Number",
        r"\u0024serde_json::private::Number",
        r"$serde_json::private::Num\u0062er",
    ] {
        for member in [r#""27""#, r#""not a number""#, "27", "null", "[]", "{}"] {
            let body = format!(r#"{{"{key}":{member}}}"#);
            for value in [
                parse::<Value>(body.as_bytes()).unwrap(),
                parse_str::<Value>(&body).unwrap(),
            ] {
                let object = value.as_object().expect("literal object remains an object");
                assert_eq!(object.len(), 1);
                assert_eq!(object["$serde_json::private::Number"].to_string(), member);
                let expected = format!(r#"{{"$serde_json::private::Number":{member}}}"#);
                assert_eq!(to_string(&value).unwrap(), expected);
                assert_eq!(to_vec(&value).unwrap(), expected.as_bytes());
            }
        }
    }
}

#[test]
fn nested_marker_objects_and_exact_numbers_remain_distinct() {
    let body = br#"{"items":[{"$serde_json::private::Number":"27","other":true},27.0000000000000001,9007199254740993,1e+400,{"nested":{"\u0024serde_json::private::Number":"27"}}]}"#;
    let value = parse::<Value>(body).unwrap();
    assert!(value["items"][0].is_object());
    assert_eq!(value["items"][0]["other"], true);
    assert_eq!(value["items"][1].to_string(), "27.0000000000000001");
    assert_eq!(value["items"][2].to_string(), "9007199254740993");
    assert_eq!(value["items"][3].to_string(), "1e+400");
    assert!(value["items"][4]["nested"].is_object());
    let wire = to_vec(&value).unwrap();
    assert_eq!(parse::<Value>(&wire).unwrap(), value);
}

#[test]
fn exact_parser_rejects_malformed_documents_and_validates_without_dom() {
    for body in [
        "",
        "{",
        r#"{"$serde_json::private::Number":"27"} false"#,
        r#"{"$serde_json::private::Number":"27",}"#,
        "[1,]",
        "[01]",
        r#"{"x":"\uD800"}"#,
    ] {
        assert!(parse::<Value>(body.as_bytes()).is_err(), "{body}");
    }
    let body = br#"{"\u0024serde_json::private::Number":"27","x":1e+400}"#;
    assert!(parse::<serde::de::IgnoredAny>(body).is_ok());
    assert!(parse::<serde::de::IgnoredAny>(b"[1,]").is_err());
}
