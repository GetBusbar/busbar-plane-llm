// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use crate::exchange::{
    arrive::arrive,
    attempt,
    reply::{At, Reply, ReplyCtx},
    shaping::Shaping,
};
use serde_json::json;

const HOST: &dyn crate::codec::cells::CodecHost = crate::codec::test_host::HOST;

fn settings(required: bool) -> Value {
    json!({
        "providers":{"p":{"protocol":"responses","model_capabilities":[
            {"models":["cache-v1"],"responses_cache_ttl":required},
            {"models":["*"],"responses_cache_ttl":false}
        ]},"other":{"protocol":"openai"}},
        "models":{"selected":{"provider":"p","upstream_model":"cache-v1"},
            "legacy":{"provider":"p","upstream_model":"cache-v0"},"other":{"provider":"other"}}
    })
}

fn request() -> Value {
    json!({"model":"selected", "input":[{"role":"user","content":[{
        "type":"input_text","text":"hello", "prompt_cache_breakpoint":{"mode":"explicit","ttl":"12h"}
    }]}], "prompt_cache_options":{"mode":"explicit","ttl":"30m","prewarm":false}})
}

#[test]
fn cache_capability_matches_the_configured_upstream_version() {
    let shape = Shaping::from_settings(&settings(true)).unwrap();
    assert!(shape.lane("selected").unwrap().caps.responses_cache_ttl);
    assert!(!shape.lane("legacy").unwrap().caps.responses_cache_ttl);
    assert!(!shape.lane("other").unwrap().caps.responses_cache_ttl);
    let mut bad = settings(true);
    bad["models"]["selected"]["protocol"] = json!("openai");
    assert!(Shaping::from_settings(&bad)
        .unwrap_err()
        .contains("responses_cache_ttl"));
    bad = settings(true);
    bad["providers"]["p"]["model_capabilities"][0]["responses_cache_ttl"] = json!("yes");
    assert!(Shaping::from_settings(&bad).is_err());
}

#[test]
fn declared_native_controls_preserve_lifetimes_breakpoints_and_prewarm() {
    let shape = Shaping::from_settings(&settings(true)).unwrap();
    let body = request();
    let bytes = body.to_string();
    let arrival = arrive("POST", "/v1/responses", &[], bytes.as_bytes(), &()).unwrap();
    let result = attempt::build(HOST, &arrival, &[], &shape, "selected", "selected").unwrap();
    let mut expected = body;
    expected["model"] = json!("cache-v1");
    assert_eq!(
        serde_json::from_slice::<Value>(&result.body).unwrap(),
        expected
    );
}

#[test]
fn native_and_cross_protocol_attempts_refuse_unsupported_cache_controls() {
    let body = request().to_string();
    let arrival = arrive("POST", "/v1/responses", &[], body.as_bytes(), &()).unwrap();
    for (required, member) in [(false, "selected"), (true, "legacy"), (true, "other")] {
        let shape = Shaping::from_settings(&settings(required)).unwrap();
        let failure = attempt::build(HOST, &arrival, &[], &shape, member, member).unwrap_err();
        assert_eq!(failure.status, 400, "{member}");
        assert!(failure.message.contains("cache"));
    }
}

#[test]
fn malformed_or_unknown_controls_never_silently_degrade() {
    let caps = LaneCaps {
        responses_cache_ttl: true,
        ..Default::default()
    };
    for options in [
        json!({"ttl":"24h"}),
        json!({"ttl":null}),
        json!({"mode":"auto"}),
        json!({"prewarm":1}),
        json!({"unknown":true}),
        json!(null),
    ] {
        let mut body = request();
        body[OPTIONS] = options;
        assert!(
            validate_request_bytes("responses", "responses", caps, Some(&body), &[]).is_err(),
            "{body}"
        );
    }
    for point in [
        json!({"mode":"implicit"}),
        json!({"ttl":"12h"}),
        json!({"mode":"explicit","ttl":"1h"}),
        json!(null),
    ] {
        let mut body = request();
        body["input"][0]["content"][0][BREAKPOINT] = point;
        assert!(
            validate_request_bytes("responses", "responses", caps, Some(&body), &[]).is_err(),
            "{body}"
        );
    }
    let mut body = request();
    body[RETENTION] = json!("24h");
    assert!(validate_request_bytes("responses", "responses", caps, Some(&body), &[]).is_err());
    let schema = json!({"tools":[{"type":"function","parameters":{"properties":{"prompt_cache_breakpoint":{"type":"string"}}}}]});
    assert!(
        validate_request_bytes("responses", "openai", LaneCaps::NONE, Some(&schema), &[]).is_ok()
    );
    let retention = json!({"prompt_cache_retention":"24h"});
    assert!(validate_request_bytes(
        "responses",
        "responses",
        LaneCaps::NONE,
        Some(&retention),
        &[]
    )
    .is_ok());
    assert!(
        validate_request_bytes("responses", "openai", LaneCaps::NONE, Some(&retention), &[])
            .is_err()
    );
}

fn response(split: bool) -> Value {
    let mut body = json!({"id":"resp-cache", "object":"response", "status":"completed", "model":"legacy-forged", "created_at":1,
        "output":[],"usage":{"input_tokens":100,"output_tokens":7,"total_tokens":107,
        "input_tokens_details":{"cached_tokens":30,"cache_write_tokens":20}}});
    if split {
        body["usage"]["input_tokens_details"]["cache_write_tokens_details"] =
            json!({"ttl_30m_tokens":0,"ttl_12h_tokens":20});
    }
    body
}

#[test]
fn required_split_is_pinned_to_lane_on_native_and_translated_replies() {
    for (required, split, translated) in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (true, false, true),
    ] {
        let shape = Shaping::from_settings(&settings(required)).unwrap();
        let path = if translated {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let arrival = arrive(
            "POST",
            path,
            &[],
            br#"{"model":"selected","input":"hi","messages":[{"role":"user","content":"hi"}]}"#,
            &(),
        )
        .unwrap();
        let ctx = ReplyCtx {
            arrived: &arrival,
            lane: shape.lane("selected").unwrap(),
            intent: attempt::StreamIntent::default(),
            passthrough: false,
            translate_cap: 65536,
        };
        let mut reply = Reply::new(HOST, &ctx, 200, &[(b"content-type", b"application/json")]);
        let wire = response(split).to_string();
        let result = reply.feed(HOST, &ctx, wire.as_bytes(), true, At::default());
        let counts = result.units.exact_counts.as_ref().unwrap();
        assert_eq!(counts.get("output").unwrap().to_string(), "7");
        if required && !split {
            assert!(!result.units.refused_counts.is_empty());
            assert!(!counts.contains_key("input") && !counts.contains_key("cache_write"));
        } else {
            assert!(
                result.units.refused_counts.is_empty(),
                "{:?}",
                result.units.refused_counts
            );
        }
        if !translated {
            assert_eq!(result.bytes.as_ref(), wire.as_bytes());
        }
    }
}

#[test]
fn streamed_and_cut_replies_cannot_settle_an_unattributed_aggregate() {
    for cut in [false, true] {
        let shape = Shaping::from_settings(&settings(true)).unwrap();
        let arrival = arrive(
            "POST",
            "/v1/responses",
            &[],
            br#"{"model":"selected","input":"hi","stream":true}"#,
            &(),
        )
        .unwrap();
        let ctx = ReplyCtx {
            arrived: &arrival,
            lane: shape.lane("selected").unwrap(),
            intent: attempt::StreamIntent {
                wants_stream: true,
                client_include_usage: false,
            },
            passthrough: false,
            translate_cap: 65536,
        };
        let mut reply = Reply::new(HOST, &ctx, 200, &[(b"content-type", b"text/event-stream")]);
        let event = format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type":"response.completed","response":response(false)})
        );
        let first = reply.feed(HOST, &ctx, event.as_bytes(), false, At::default());
        assert!(!first
            .units
            .exact_counts
            .as_ref()
            .is_some_and(|counts| counts.contains_key("cache_write")));
        let result = if cut {
            reply.cut(HOST, &ctx, true)
        } else {
            reply.feed(HOST, &ctx, b"", true, At::default())
        };
        assert!(!result.units.refused_counts.is_empty(), "cut={cut}");
        assert!(!result
            .units
            .exact_counts
            .as_ref()
            .unwrap()
            .contains_key("cache_write"));
    }
}

#[test]
fn escaped_control_names_cannot_bypass_native_validation() {
    let wire = br#"{"model":"selected","input":"hi","prompt_cache_\u006fptions":{"ttl":"12h"}}"#;
    assert!(validate_request_bytes("responses", "responses", LaneCaps::NONE, None, wire).is_err());
    let shape = Shaping::from_settings(&settings(false)).unwrap();
    let arrival = arrive("POST", "/v1/responses", &[], wire, &()).unwrap();
    assert_eq!(
        attempt::build(HOST, &arrival, &[], &shape, "selected", "selected")
            .unwrap_err()
            .status,
        400
    );
}
