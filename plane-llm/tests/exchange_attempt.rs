// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! One attempt's far-end request.

mod common;

use common::TestHost;
use serde_json::json;

use busbar_plane_llm::exchange::arrive::arrive;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::*;
use busbar_plane_llm::exchange::shaping::Shaping;
use serde_json::Value;

/// The handle every codec call here is handed.
const HOST: &dyn busbar_contract::codec::CodecHost = &TestHost;

fn shaping() -> Shaping {
    Shaping::from_settings(&json!({
        "providers": {
            "oai": { "protocol": "openai", "base_url": "https://api.example" },
            "ant": { "protocol": "anthropic", "base_url": "https://anthropic.example" },
            "ant-key": { "protocol": "anthropic", "base_url": "https://az.example", "auth": "api-key" },
            "ant-bearer": { "protocol": "anthropic", "base_url": "https://b.example", "auth": "bearer" },
            "goo": { "protocol": "gemini", "base_url": "https://gemini.example" },
            "fixed": { "protocol": "openai", "base_url": "https://x.example", "path": "/custom/chat" }
        },
        "models": {
            "gpt": { "provider": "oai" },
            "gpt-alias": { "provider": "oai", "upstream_model": "gpt-4o" },
            "claude": { "provider": "ant", "default_max_tokens": 321 },
            "claude-key": { "provider": "ant-key", "default_max_tokens": 321 },
            "claude-bearer": { "provider": "ant-bearer", "default_max_tokens": 321 },
            "gem": { "provider": "goo", "upstream_model": "gemini-pro" },
            "fixed": { "provider": "fixed" }
        },
        "pools": { "p": { "members": ["gpt", "claude"] } }
    }))
    .expect("reads")
}

fn head(pairs: &[(&'static str, &'static str)]) -> Vec<(&'static [u8], &'static [u8])> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
        .collect()
}

fn arrived(target: &str, h: &[(&[u8], &[u8])], body: &str) -> Arrived {
    arrive("POST", target, h, body.as_bytes(), &()).expect("arrives")
}

fn field<'a>(r: &'a FarRequest, name: &str) -> Vec<&'a [u8]> {
    r.fields
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.as_slice())
        .collect()
}

#[test]
fn a_same_dialect_body_the_far_end_would_not_change_goes_out_as_the_callers_bytes() {
    let h = head(&[("content-type", "application/json")]);
    let body = r#"{"model":"gpt","messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert!(r.pristine);
    assert_eq!(
        r.body,
        body.as_bytes(),
        "byte for byte, whitespace and key order kept"
    );
    assert_eq!(r.verb, "POST");
    assert_eq!(r.target, "/v1/chat/completions");
}

#[test]
fn the_wire_model_is_written_when_it_differs() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt-alias","messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt-alias").expect("built");
    assert!(!r.pristine);
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert_eq!(v["model"], "gpt-4o");

    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"z":1.50, "model":"gpt-alias","messages":[],"a":{"y":1,"x":2}}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt-alias").expect("built");
    assert_eq!(
        r.body,
        br#"{"z":1.50, "model":"gpt-4o","messages":[],"a":{"y":1,"x":2}}"#
    );
}

#[test]
fn a_cross_dialect_request_is_written_in_the_far_ends_dialect() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"claude","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    assert!(!r.pristine);
    assert_eq!(r.target, "/v1/messages");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert_eq!(v["model"], "claude");
    assert_eq!(
        v["max_tokens"], 321,
        "the lane's own default, the far end requires one"
    );
    assert_eq!(v["messages"][0]["role"], "user");
}

#[test]
fn a_same_dialect_caller_s_fields_all_go_out_but_the_governed_ones() {
    let h = head(&[
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "a"),
        ("anthropic-beta", "b"),
        ("X-Client-Trace", "abc"),
        ("authorization", "Bearer never-forwarded"),
        ("x-api-key", "never-forwarded"),
    ]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude","max_tokens":5,"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    let names: Vec<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "anthropic-version",
            "content-type",
            "user-agent",
            "accept",
            "anthropic-beta",
            "anthropic-beta",
            "x-client-trace"
        ],
        "busbar's own fields at their positions (the caller's values where it sent one), then every \
         other caller field but the credential, as 1.5.5's header map laid them"
    );
    assert_eq!(
        field(&r, "user-agent"),
        [b"Anthropic/Python 0.39.0".as_slice()],
        "a caller that sent no user-agent: the native client's (1.5.5's bytes, Q10)"
    );
    assert_eq!(field(&r, "accept"), [b"application/json".as_slice()]);
    assert_eq!(
        field(&r, "anthropic-beta"),
        [b"a".as_slice(), b"b".as_slice()]
    );
    assert_eq!(field(&r, "x-client-trace"), [b"abc".as_slice()]);
}

#[test]
fn the_caller_s_value_replaces_busbar_s_native_default() {
    let h = head(&[
        ("content-type", "application/json"),
        ("user-agent", "my-sdk/1.0"),
    ]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude","max_tokens":5,"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    assert_eq!(field(&r, "user-agent"), [b"my-sdk/1.0".as_slice()]);
}

#[test]
fn the_caller_s_tenant_selectors_never_go_out() {
    let h = head(&[
        ("content-type", "application/json"),
        ("OpenAI-Organization", "org-caller"),
        ("OpenAI-Project", "proj-caller"),
    ]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(field(&r, "openai-organization").len(), 0);
    assert_eq!(field(&r, "openai-project").len(), 0);
}

#[test]
fn a_translated_route_forwards_no_caller_field() {
    let h = head(&[
        ("content-type", "application/json"),
        ("anthropic-beta", "a"),
        ("openai-beta", "assistants=v2"),
        ("x-client-trace", "abc"),
    ]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude","max_tokens":5,"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    let names: Vec<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["content-type", "user-agent", "accept"]);
    assert_eq!(
        field(&r, "user-agent"),
        [b"OpenAI/Python 1.54.0".as_slice()],
        "written in the far dialect, as its native client"
    );
}

#[test]
fn a_same_dialect_unknown_body_member_goes_out() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt-alias","messages":[],"x_vendor_flag":true}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt-alias").expect("built");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert_eq!(v["model"], "gpt-4o", "the mapped model");
    assert_eq!(v["x_vendor_flag"], true, "the unknown member, unchanged");
}

/// A body-model caller that smuggles busbar's own array-stream router key into its body is not a

#[test]
fn a_body_model_callers_smuggled_router_key_never_reaches_the_far_end() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","stream":true,"__busbar_gemini_json_array":true,"messages":[]}"#,
    );
    assert!(
        !a.path_model.as_ref().is_some_and(|p| p.json_array),
        "a body key never makes a JSON-array caller"
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert!(!r.pristine, "the router key is a governed member");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert!(v.get("__busbar_gemini_json_array").is_none(), "{v}");
    assert_eq!(
        v["stream"], true,
        "a body-model far end keeps the caller's stream"
    );
}

#[test]
fn a_translated_route_drops_a_member_the_far_dialect_cannot_carry() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"claude","messages":[{"role":"user","content":"hi"}],"x_vendor_flag":true}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert!(v.get("x_vendor_flag").is_none());
}

#[test]
fn a_streamed_request_asks_a_far_end_that_reports_usage_only_when_asked() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","stream":true,"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(
        r.body,
        br#"{"stream_options":{"include_usage":true},"model":"gpt","stream":true,"messages":[]}"#
    );
    assert_eq!(field(&r, "accept"), [b"text/event-stream".as_slice()]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","stream":true,"stream_options":7,"messages":[]}"#,
    );
    let refused = build(HOST, &a, &h, &shaping(), "p", "gpt").expect_err("refused");
    assert_eq!(refused.status, 400);
    assert_eq!(refused.message, DETAIL_STREAM_OPTIONS_NOT_OBJECT);
}

#[test]
fn the_far_ends_path_is_its_dialects_own_or_the_providers_fixed_one() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gem","stream":true,"messages":[{"role":"user","content":"x"}]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gem").expect("built");
    assert!(
        r.target.contains("gemini-pro") && r.target.contains("streamGenerateContent"),
        "{}",
        r.target
    );
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"fixed","messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "fixed").expect("built");
    assert_eq!(r.target, "/custom/chat");
}

#[test]
fn a_path_outside_the_unreserved_set_is_percent_encoded_on_the_wire_and_twice_for_a_signature() {
    assert_eq!(
        wire_and_canonical_path("/model/a.b-c_d~e/converse"),
        (
            "/model/a.b-c_d~e/converse".to_string(),
            "/model/a.b-c_d~e/converse".to_string()
        )
    );
    assert_eq!(
        wire_and_canonical_path("/model/x:0/converse?q=1"),
        (
            "/model/x%3A0/converse?q=1".to_string(),
            "/model/x%253A0/converse".to_string()
        )
    );
}

#[test]
fn a_member_the_generation_does_not_hold_is_the_callers_500() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","messages":[]}"#,
    );
    let refused = build(HOST, &a, &h, &shaping(), "p", "nobody").expect_err("refused");
    assert_eq!((refused.status, refused.envelope), (500, "openai"));
    assert_eq!(refused.message, DETAIL_INTERNAL_ERROR);
}

#[test]
fn the_usage_ask_is_a_splice_after_the_opening_brace_or_in_place() {
    let out = try_inject_stream_include_usage(br#" {"a":1}"#.to_vec()).expect("asked");
    assert_eq!(out, br#" {"stream_options":{"include_usage":true},"a":1}"#);
    let out =
        try_inject_stream_include_usage(br#"{"stream_options":null}"#.to_vec()).expect("asked");
    assert_eq!(out, br#"{"stream_options":{"include_usage":true}}"#);
    let out = try_inject_stream_include_usage(br#"{"z":1, "stream_options":{"b":2}}"#.to_vec())
        .expect("asked");
    assert_eq!(
        out,
        br#"{"z":1, "stream_options":{"include_usage":true,"b":2}}"#
    );
    let out = try_inject_stream_include_usage(b"[1]".to_vec()).expect("unchanged");
    assert_eq!(out, b"[1]");
    assert!(try_inject_stream_include_usage(br#"{"stream_options":"x"}"#.to_vec()).is_err());
}

/// BUSBAR IS INVISIBLE TO UPSTREAMS, THE URL TOO: a same-dialect attempt carries the caller's own

#[test]
fn the_callers_query_goes_out_on_a_same_dialect_attempt() {
    assert_eq!(
        with_caller_query(
            "gemini",
            "/v1beta/models/m:streamGenerateContent?alt=sse",
            "key=secret&alt=json&trace=a%2Fb"
        ),
        Some("/v1beta/models/m:streamGenerateContent?alt=sse&trace=a%2Fb".to_string())
    );
    assert_eq!(
        with_caller_query("openai", "/v1/chat/completions", "trace=1&beta=true"),
        Some("/v1/chat/completions?trace=1&beta=true".to_string())
    );
    assert_eq!(with_caller_query("gemini", "/x", "key=secret"), None);
    let h = head(&[("content-type", "application/json")]);
    let a = arrive(
        "POST",
        "/v1/messages?beta=true",
        &h,
        br#"{"model":"claude","max_tokens":5,"messages":[]}"#,
        &(),
    )
    .expect("arrives");
    let same = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    assert_eq!(same.target, "/v1/messages?beta=true");
    let crossed = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(crossed.target, "/v1/chat/completions");
}

/// A signed dialect's attempt carries no caller query until its signer signs the canonical query

#[test]
fn a_signed_dialects_attempt_carries_no_query_yet() {
    assert_eq!(
        with_caller_query("bedrock", "/model/m/converse", "trace=1"),
        None
    );
}

/// A path-model same-dialect relay sends the caller's bytes: the `model` and `stream` the arrival

#[test]
fn a_path_model_same_dialect_request_reaches_the_far_end_as_the_caller_wrote_it() {
    let h = head(&[("content-type", "application/json")]);
    let body = "{ \"zz_never_heard_of\": {\"b\":1,\"a\":2},\n  \"contents\": [ {\"parts\":[{\"text\":\"hi\"}]} ], \"n\": 1.50 }";
    let a = arrive(
        "POST",
        "/v1beta/models/gemini-pro:streamGenerateContent?alt=sse",
        &h,
        body.as_bytes(),
        &(),
    )
    .expect("arrives");
    let r = build(HOST, &a, &h, &shaping(), "p", "gem").expect("built");
    assert_eq!(String::from_utf8_lossy(&r.body), body);
}

/// A conversation history carrying a `custom` tool call (a type the IR does not model) is relayed

#[test]
fn a_history_with_a_custom_tool_call_relays_byte_identical() {
    let h = head(&[("content-type", "application/json")]);
    let body = r#"{"model":"gpt","messages":[{"role":"user","content":"go"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_c","type":"custom","custom":{"name":"grammar","input":"x = 1"}}]},{"role":"tool","tool_call_id":"call_c","content":"ok"}],"tools":[{"type":"custom","custom":{"name":"grammar"}}]}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(String::from_utf8_lossy(&r.body), body);
}

/// TENANT SELECTORS COME FROM BUSBAR'S CONFIG: a provider's `OpenAI-Organization` /

#[test]
fn the_providers_tenant_goes_out_and_the_callers_does_not() {
    let shaping = Shaping::from_settings(&json!({
        "providers": {
            "oai": {
                "protocol": "openai",
                "base_url": "https://api.example",
                "OpenAI-Organization": "org-cfg",
                "openai-project": "proj-cfg"
            }
        },
        "models": { "gpt": { "provider": "oai" } },
        "pools": { "p": { "members": ["gpt"] } }
    }))
    .expect("reads");
    let h = head(&[
        ("content-type", "application/json"),
        ("openai-organization", "org-caller"),
    ]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping, "p", "gpt").expect("built");
    assert_eq!(field(&r, "openai-organization"), [b"org-cfg".as_slice()]);
    assert_eq!(field(&r, "openai-project"), [b"proj-cfg".as_slice()]);
}

/// THE DIALECT'S STATIC FIELDS (anthropic declares

#[test]
fn no_client_header_leaves_egress_unchanged() {
    let none = head(&[]);
    let body = r#"{"model":"claude","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/messages", &none, body);
    let r = build(HOST, &a, &none, &shaping(), "p", "claude").expect("built");
    assert_eq!(field(&r, "anthropic-version"), [b"2023-06-01".as_slice()]);
    assert_eq!(
        field(&r, "user-agent"),
        [b"Anthropic/Python 0.39.0".as_slice()]
    );
    let names: Vec<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["anthropic-version", "content-type", "user-agent", "accept"]
    );

    let openai = r#"{"model":"claude","messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/chat/completions", &none, openai);
    let r = build(HOST, &a, &none, &shaping(), "p", "claude").expect("built");
    assert_eq!(
        field(&r, "anthropic-version"),
        [b"2023-06-01".as_slice()],
        "a translated route into anthropic carries it too"
    );

    let a = arrived("/v1/messages", &none, &body.replace("claude", "claude-key"));
    let r = build(HOST, &a, &none, &shaping(), "p", "claude-key").expect("built");
    assert!(
        field(&r, "anthropic-version").is_empty(),
        "an api-key override declares no static field"
    );
    let a = arrived(
        "/v1/messages",
        &none,
        &body.replace("claude", "claude-bearer"),
    );
    let r = build(HOST, &a, &none, &shaping(), "p", "claude-bearer").expect("built");
    assert_eq!(field(&r, "anthropic-version"), [b"2023-06-01".as_slice()]);

    let own = head(&[("anthropic-version", "2024-01-01")]);
    let a = arrived("/v1/messages", &own, body);
    let r = build(HOST, &a, &own, &shaping(), "p", "claude").expect("built");
    assert_eq!(
        field(&r, "anthropic-version"),
        [b"2024-01-01".as_slice()],
        "the same-dialect caller's own value wins"
    );

    let a = arrived(
        "/v1/chat/completions",
        &none,
        &openai.replace("claude", "gpt"),
    );
    let r = build(HOST, &a, &none, &shaping(), "p", "gpt").expect("built");
    assert!(
        field(&r, "anthropic-version").is_empty(),
        "openai declares none"
    );
}

/// THE PREVIOUS RELEASE'S HEADER-MAP ORDER (oracle `egress.auth|cred-identity|*`, `route.529|*`): a

#[test]
fn a_same_dialect_callers_fields_keep_busbars_positions() {
    let h = head(&[
        ("user-agent", "Anthropic/Python 0.39.0"),
        ("accept", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("content-type", "application/json"),
        ("x-trace", "a"),
        ("x-trace", "b"),
    ]);
    let body = r#"{"model":"claude","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/messages", &h, body);
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    let names: Vec<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "anthropic-version",
            "content-type",
            "user-agent",
            "accept",
            "x-trace",
            "x-trace"
        ]
    );
    assert_eq!(field(&r, "x-trace"), [b"a".as_slice(), b"b".as_slice()]);
}

/// THE `accept` FIELD IS BUSBAR'S OWN (1.5.5's bytes: the oracle's same-dialect cells, e.g.

#[test]
fn a_same_dialect_callers_accept_never_replaces_busbars_own() {
    let h = head(&[("accept", "*/*"), ("x-trace", "a")]);
    let body = r#"{"model":"gpt","messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(field(&r, "accept"), [b"application/json".as_slice()]);
    assert_eq!(field(&r, "x-trace"), [b"a".as_slice()]);
    let streamed = r#"{"model":"gpt","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let a = arrived("/v1/chat/completions", &h, streamed);
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(field(&r, "accept"), [b"text/event-stream".as_slice()]);
}

use busbar_contract::codec::IngressReject;
use busbar_contract::operation::OpVerb;
use busbar_plane_llm::exchange::reply::whole::{self, WholeCtx, WholeEnd};
use busbar_plane_llm::exchange::shaping::Lane;

/// The six dialects a far end speaks.
const DIALECTS: [&str; 6] = [
    "openai",
    "anthropic",
    "gemini",
    "cohere",
    "bedrock",
    "responses",
];

/// One provider of `dialect` serving one model `member` (sent as `wire` when given), in pool `p`;

fn one_lane(
    dialect: &str,
    member: &str,
    wire: Option<&str>,
    provider_extra: &[(&str, &str)],
) -> Shaping {
    let mut provider = serde_json::Map::new();
    provider.insert("protocol".into(), json!(dialect));
    provider.insert("base_url".into(), json!("https://far.example"));
    for (k, v) in provider_extra {
        provider.insert((*k).to_string(), json!(v));
    }
    let mut model = serde_json::Map::new();
    model.insert("provider".into(), json!("far"));
    if let Some(w) = wire {
        model.insert("upstream_model".into(), json!(w));
    }
    let mut models = serde_json::Map::new();
    models.insert(member.to_string(), Value::Object(model));
    Shaping::from_settings(&json!({
        "providers": { "far": Value::Object(provider) },
        "models": Value::Object(models),
        "pools": { "p": { "members": [member] } }
    }))
    .expect("reads")
}

fn far_json(r: &FarRequest) -> Value {
    busbar_plane_llm::codec::json::parse(&r.body).expect("the far body is JSON")
}

/// Rehomes `crates/busbar-llm/src/engine/tests/client_header_forwarding_tests.rs::same_dialect_anthropic_forwards_every_client_header`.

#[test]
fn a_same_dialect_anthropic_callers_own_version_beta_and_trace_go_out() {
    let h = head(&[
        ("content-type", "application/json"),
        (
            "anthropic-beta",
            "prompt-caching-2024-07-31,message-batches-2024-09-24",
        ),
        ("anthropic-version", "2020-01-01-clienttest"),
        ("x-client-trace", "abc"),
    ]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude","messages":[{"role":"user","content":"hi"}],"max_tokens":100}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    assert_eq!(
        field(&r, "anthropic-version"),
        [b"2020-01-01-clienttest".as_slice()],
        "the caller's version wins over busbar's 2023-06-01"
    );
    assert_eq!(
        field(&r, "anthropic-beta"),
        [b"prompt-caching-2024-07-31,message-batches-2024-09-24".as_slice()]
    );
    assert_eq!(field(&r, "x-client-trace"), [b"abc".as_slice()]);
}

/// Rehomes `crates/busbar-llm/src/engine/tests/client_header_forwarding_tests.rs::same_dialect_openai_forwards_headers_and_unknown_body_fields`.

#[test]
fn a_same_dialect_openai_callers_fields_and_unknown_member_go_out() {
    let h = head(&[
        ("content-type", "application/json"),
        ("openai-beta", "assistants=v2"),
        ("x-client-trace", "abc"),
        ("user-agent", "OpenAI/Python 9.9.9"),
    ]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","messages":[{"role":"user","content":"hi"}],"x_vendor_flag":true}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    assert_eq!(field(&r, "openai-beta"), [b"assistants=v2".as_slice()]);
    assert_eq!(field(&r, "x-client-trace"), [b"abc".as_slice()]);
    assert_eq!(
        field(&r, "user-agent"),
        [b"OpenAI/Python 9.9.9".as_slice()],
        "the caller's user-agent wins over busbar's native default"
    );
    assert_eq!(far_json(&r)["x_vendor_flag"], json!(true));
}

/// Rehomes `crates/busbar-llm/src/engine/tests/client_header_forwarding_tests.rs::neutral_apply_replaces_then_appends`.

#[test]
fn a_callers_two_values_replace_busbars_default_then_append() {
    let h = head(&[
        ("content-type", "application/json"),
        ("user-agent", "first"),
        ("user-agent", "second"),
    ]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude","max_tokens":5,"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    assert_eq!(
        field(&r, "user-agent"),
        [b"first".as_slice(), b"second".as_slice()]
    );
    let names: Vec<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "anthropic-version",
            "content-type",
            "user-agent",
            "user-agent",
            "accept"
        ],
        "busbar's position, the caller's values"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/client_header_forwarding_tests.rs::the_tenant_comes_from_config_never_from_the_caller`.

#[test]
fn the_tenant_comes_from_config_on_every_route_and_never_from_the_caller() {
    let caller = |ct: &'static str| {
        head(&[
            ("content-type", ct),
            ("openai-organization", "org-caller"),
            ("openai-project", "proj-caller"),
        ])
    };
    let tenant = [
        ("OpenAI-Organization", "org-cfg"),
        ("OpenAI-Project", "proj-cfg"),
    ];
    let seen = |r: &FarRequest| {
        (
            field(r, "openai-organization")
                .first()
                .map(|v| String::from_utf8_lossy(v).into_owned()),
            field(r, "openai-project")
                .first()
                .map(|v| String::from_utf8_lossy(v).into_owned()),
        )
    };
    let configured = (Some("org-cfg".to_string()), Some("proj-cfg".to_string()));
    let openai_body = r#"{"model":"test-model","messages":[{"role":"user","content":"hi"}]}"#;
    let anthropic_body =
        r#"{"model":"test-model","messages":[{"role":"user","content":"hi"}],"max_tokens":100}"#;
    let h = caller("application/json");

    let s = one_lane("openai", "test-model", None, &tenant);
    let a = arrived("/v1/chat/completions", &h, openai_body);
    let r = build(HOST, &a, &h, &s, "p", "test-model").expect("built");
    assert_eq!(seen(&r), configured, "openai caller, openai lane");

    let a = arrived("/v1/messages", &h, anthropic_body);
    let r = build(HOST, &a, &h, &s, "p", "test-model").expect("built");
    assert_eq!(seen(&r), configured, "anthropic caller, openai lane");

    let s = one_lane("openai", "test-model", None, &[]);
    let a = arrived("/v1/chat/completions", &h, openai_body);
    let r = build(HOST, &a, &h, &s, "p", "test-model").expect("built");
    assert_eq!(seen(&r), (None, None), "openai lane, no tenant configured");

    let refused = Shaping::from_settings(&json!({
        "providers": { "far": {
            "protocol": "anthropic",
            "OpenAI-Organization": "org-cfg"
        } },
        "models": { "test-model": { "provider": "far" } },
        "pools": { "p": { "members": ["test-model"] } }
    }))
    .expect_err("an anthropic provider takes no OpenAI selector");
    assert!(
        refused.contains("unknown key `OpenAI-Organization`"),
        "{refused}"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/egress_target_tests.rs::egress_targets_match_reference_composition_all_protocols`.

#[test]
fn every_dialects_chat_target_is_its_released_path_both_stream_intents() {
    let h = head(&[("content-type", "application/json")]);
    let table: [(&str, bool, &str, &str); 12] = [
        (
            "openai",
            false,
            "/v1/chat/completions",
            "/v1/chat/completions",
        ),
        (
            "openai",
            true,
            "/v1/chat/completions",
            "/v1/chat/completions",
        ),
        ("anthropic", false, "/v1/messages", "/v1/messages"),
        ("anthropic", true, "/v1/messages", "/v1/messages"),
        (
            "gemini",
            false,
            "/v1beta/models/m-1%3AgenerateContent",
            "/v1beta/models/m-1%253AgenerateContent",
        ),
        (
            "gemini",
            true,
            "/v1beta/models/m-1%3AstreamGenerateContent",
            "/v1beta/models/m-1%253AstreamGenerateContent",
        ),
        ("cohere", false, "/v2/chat", "/v2/chat"),
        ("cohere", true, "/v2/chat", "/v2/chat"),
        (
            "bedrock",
            false,
            "/model/m-1/converse",
            "/model/m-1/converse",
        ),
        (
            "bedrock",
            true,
            "/model/m-1/converse-stream",
            "/model/m-1/converse-stream",
        ),
        ("responses", false, "/v1/responses", "/v1/responses"),
        ("responses", true, "/v1/responses", "/v1/responses"),
    ];
    for (dialect, stream, wire, canonical) in table {
        let s = one_lane(dialect, "m-1", None, &[]);
        let body = format!(
            r#"{{"model":"m-1","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let a = arrived("/v1/chat/completions", &h, &body);
        let r = build(HOST, &a, &h, &s, "p", "m-1").unwrap_or_else(|e| panic!("{dialect}: {e:?}"));
        assert_eq!(r.target, wire, "{dialect} stream {stream}");
        let lane = s.lane("m-1").expect("the lane");
        let path = upstream_path(lane, OpVerb::CHAT, stream).expect("a path");
        assert_eq!(
            wire_and_canonical_path(&path),
            (wire.to_string(), canonical.to_string()),
            "{dialect} stream {stream}"
        );
    }
}

/// Rehomes `crates/busbar-llm/src/engine/tests/egress_target_tests.rs::egress_targets_honor_azure_path_override_with_query`.

#[test]
fn a_provider_path_with_a_query_is_the_target_verbatim() {
    let fixed = "/openai/deployments/gpt-4o/chat/completions?api-version=2024-06-01";
    let s = one_lane("openai", "gpt-4o", None, &[("path", fixed)]);
    let h = head(&[("content-type", "application/json")]);
    for stream in [false, true] {
        let body = format!(r#"{{"model":"gpt-4o","stream":{stream},"messages":[]}}"#);
        let a = arrived("/v1/chat/completions", &h, &body);
        let r = build(HOST, &a, &h, &s, "p", "gpt-4o").expect("built");
        assert_eq!(r.target, fixed, "stream {stream}");
    }
    let lane = s.lane("gpt-4o").expect("the lane");
    let path = upstream_path(lane, OpVerb::CHAT, false).expect("a path");
    assert_eq!(
        wire_and_canonical_path(&path),
        (
            fixed.to_string(),
            "/openai/deployments/gpt-4o/chat/completions".to_string()
        )
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_egress_accept_matches_native_sdk`.

#[test]
fn every_far_end_reads_its_native_sdks_accept() {
    let h = head(&[("content-type", "application/json")]);
    for dialect in DIALECTS {
        let s = one_lane(dialect, "m-1", None, &[]);
        for stream in [false, true] {
            let body = format!(
                r#"{{"model":"m-1","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
            );
            let a = arrived("/v1/chat/completions", &h, &body);
            let r = build(HOST, &a, &h, &s, "p", "m-1").expect("built");
            let want: &[u8] = match (dialect, stream) {
                ("bedrock", true) => b"application/vnd.amazon.eventstream",
                (_, true) => b"text/event-stream",
                (_, false) => b"application/json",
            };
            assert_eq!(field(&r, "accept"), [want], "{dialect} stream {stream}");
        }
    }
    let refused = Shaping::from_settings(&json!({
        "providers": { "far": { "protocol": "mystery", "base_url": "https://far.example" } },
        "models": { "m-1": { "provider": "far" } }
    }));
    assert!(refused.is_err(), "an unknown dialect is never a lane");
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_forward_once_cross_protocol_strips_source_only_extra_keys`.

#[test]
fn an_openai_callers_source_only_keys_never_reach_an_anthropic_far_end() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("anthropic", "claude-3", None, &[]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"leastbad","messages":[{"role":"user","content":"hi"}],"max_tokens":16,"logprobs":true,"top_logprobs":5,"n":1}"#,
    );
    let r = build(HOST, &a, &h, &s, "p", "claude-3").expect("the attempt goes out");
    let v = far_json(&r);
    let obj = v.as_object().expect("an object");
    for key in ["logprobs", "top_logprobs", "n"] {
        assert!(!obj.contains_key(key), "{key} leaked: {v}");
    }
    assert!(obj.contains_key("messages"), "{v}");
}

fn openai_ok_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "model": "glm-4.5",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi there"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    }))
    .expect("JSON")
}

fn anthropic_ok_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "msg_x",
        "type": "message",
        "role": "assistant",
        "model": "claude-x",
        "content": [{"type": "text", "text": "hi there"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    }))
    .expect("JSON")
}

fn whole_ctx<'a>(a: &'a Arrived, egress: &'a str, model: &'a str) -> WholeCtx<'a> {
    WholeCtx {
        ingress: a.dialect,
        egress,
        operation: a.operation,
        model,
        wants_stream: false,
        json_array: false,
        request: a.parsed.as_ref(),
        now_s: 1_752_000_000,
        elapsed_ms: Some(12),
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_cohere_ingress_to_openai_backend`.

#[test]
fn a_cohere_caller_reaches_an_openai_far_end_and_back() {
    let h = head(&[
        ("content-type", "application/json"),
        ("authorization", "Bearer t"),
    ]);
    let s = one_lane("openai", "glm-4.5", None, &[]);
    let a = arrived(
        "/v2/chat",
        &h,
        r#"{"model":"co","messages":[{"role":"user","content":"hello"}]}"#,
    );
    assert_eq!(a.dialect, "cohere");
    let r = build(HOST, &a, &h, &s, "co", "glm-4.5").expect("built");
    assert_eq!(r.target, "/v1/chat/completions");
    let v = far_json(&r);
    assert!(v.get("messages").is_some_and(Value::is_array), "{v}");
    assert_eq!(v["model"], "glm-4.5");
    assert!(
        field(&r, "authorization").is_empty(),
        "the caller's credential never goes out"
    );
    let w = whole::translate(
        HOST,
        &whole_ctx(&a, "openai", "glm-4.5"),
        200,
        &openai_ok_body(),
    );
    assert_eq!(w.end, WholeEnd::Delivered, "{:?}", w.refused);
    assert_eq!(w.answer.status, 200);
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_responses_ingress_to_anthropic_backend`.

#[test]
fn a_responses_caller_reaches_an_anthropic_far_end_and_back() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("anthropic", "claude-x", None, &[]);
    let a = arrived(
        "/v1/responses",
        &h,
        r#"{"model":"re","input":"hello","max_tokens":16}"#,
    );
    assert_eq!(a.dialect, "responses");
    let r = build(HOST, &a, &h, &s, "re", "claude-x").expect("built");
    assert_eq!(r.target, "/v1/messages");
    let v = far_json(&r);
    assert!(v.get("messages").is_some_and(Value::is_array), "{v}");
    let w = whole::translate(
        HOST,
        &whole_ctx(&a, "anthropic", "claude-x"),
        200,
        &anthropic_ok_body(),
    );
    assert_eq!(w.end, WholeEnd::Delivered, "{:?}", w.refused);
    assert_eq!(w.answer.status, 200);
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_same_protocol_passthrough_strips_shim_keys`.

#[test]
fn a_bedrock_same_dialect_relay_carries_no_routing_members() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("bedrock", "foo", None, &[("path", "/v1/messages")]);
    let body = r#"{"messages":[{"role":"user","content":[{"text":"hello"}]}]}"#;
    let a = arrived("/model/foo/converse", &h, body);
    let r = build(HOST, &a, &h, &s, "foo", "foo").expect("built");
    assert_eq!(r.target, "/v1/messages");
    let v = far_json(&r);
    assert!(v.get("model").is_none(), "{v}");
    assert!(v.get("stream").is_none(), "{v}");
    assert!(v.get("messages").is_some(), "{v}");
    assert_eq!(String::from_utf8_lossy(&r.body), body);
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_gemini_json_array_shim_not_leaked_cross_protocol`.

#[test]
fn a_gemini_json_array_callers_router_key_never_reaches_a_body_model_far_end() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("openai", "foo", None, &[]);
    let a = arrived(
        "/v1beta/models/foo:streamGenerateContent",
        &h,
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
    );
    assert!(a.path_model.as_ref().is_some_and(|p| p.json_array));
    let r = build(HOST, &a, &h, &s, "foo", "foo").expect("built");
    let v = far_json(&r);
    assert!(v.get("__busbar_gemini_json_array").is_none(), "{v}");
    assert_eq!(v.get("stream").and_then(Value::as_bool), Some(true), "{v}");
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_reject_response_tests.rs::unsupported_sub_op_rejects_with_404_naming_op_and_model`.

#[test]
fn an_unsupported_sub_operation_is_a_404_naming_the_operation_and_the_model() {
    let refused = ingress_reject(
        "openai",
        &IngressReject::UnsupportedSubOp {
            op: OpVerb::IMAGE,
            model: "dall-e-2".into(),
        },
    );
    assert_eq!(refused.status, 404);
    assert_eq!(refused.kind, "not_found_error");
    let r = busbar_plane_llm::exchange::refuse::answered(HOST, &refused);
    assert_eq!(r.status, 404);
    let v: Value = serde_json::from_slice(&r.body).expect("JSON");
    let detail = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        detail.contains("image") && detail.contains("dall-e-2"),
        "{detail}"
    );
    assert!(
        !detail.contains("Verb") && !detail.contains("OpShape") && !detail.contains("Invoke"),
        "{detail}"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/ingress_reject_response_tests.rs::bad_request_reject_keeps_the_unchanged_generic_400`.

#[test]
fn a_bad_request_reject_is_the_generic_400() {
    let refused = ingress_reject("openai", &IngressReject::BadRequest("x".into()));
    assert_eq!(refused.status, 400);
    let r = busbar_plane_llm::exchange::refuse::answered(HOST, &refused);
    assert_eq!(r.status, 400);
    let v: Value = serde_json::from_slice(&r.body).expect("JSON");
    assert_eq!(
        v["error"]["message"].as_str(),
        Some("We could not process the content of your request.")
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/lazy_body_tests.rs::a_same_dialect_hop_is_the_callers_bytes_with_governed_splices`.

#[test]
fn a_same_dialect_hop_is_the_callers_bytes_with_the_governed_splices_only() {
    let cases: [(&str, &str, &str, &str, &str); 8] = [
        (
            "openai",
            "alias",
            "gpt-4o-real",
            r#"{"z":1.50, "model":"alias","never_heard_of":{"y":1,"x":2},"messages":[]}"#,
            r#"{"z":1.50, "model":"gpt-4o-real","never_heard_of":{"y":1,"x":2},"messages":[]}"#,
        ),
        (
            "anthropic",
            "claude-3",
            "claude-3",
            r#"{"max_tokens":7,"model":"claude-3","messages":[],"zz":{"b":1,"a":2}}"#,
            r#"{"max_tokens":7,"model":"claude-3","messages":[],"zz":{"b":1,"a":2}}"#,
        ),
        (
            "openai",
            "gpt-4o",
            "gpt-4o",
            r#"{"model":"gpt-4o", "__busbar_gemini_json_array":true,"z":1.50,"a":[]}"#,
            r#"{"model":"gpt-4o","z":1.50,"a":[]}"#,
        ),
        (
            "gemini",
            "url-model-x",
            "url-model-x",
            r#"{"model":"url-model-x","stream":false,"contents":[{"role":"user","parts":[{"text":"hi"}]}],"b":1,"a":2}"#,
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"b":1,"a":2}"#,
        ),
        (
            "cohere",
            "alias",
            "command-r",
            r#"{"zz_never_heard_of":[1.50,{"b":1,"a":2}], "model":"alias","messages":[]}"#,
            r#"{"zz_never_heard_of":[1.50,{"b":1,"a":2}], "model":"command-r","messages":[]}"#,
        ),
        (
            "responses",
            "gpt-4o",
            "gpt-4o",
            r#"{"input":"hi", "model":"gpt-4o","zz_never_heard_of":{"b":1,"a":2}}"#,
            r#"{"input":"hi", "model":"gpt-4o","zz_never_heard_of":{"b":1,"a":2}}"#,
        ),
        (
            "bedrock",
            "anthropic.claude",
            "anthropic.claude",
            r#"{"model":"anthropic.claude","stream":true,"messages":[{"role":"user","content":[{"text":"hi"}]}], "zz_never_heard_of":{"b":1,"a":2}}"#,
            r#"{"messages":[{"role":"user","content":[{"text":"hi"}]}], "zz_never_heard_of":{"b":1,"a":2}}"#,
        ),
        ("openai", "m", "m", "[1,2,3]", "[1,2,3]"),
    ];
    for (dialect, member, wire, carried, want) in cases {
        let s = one_lane(dialect, member, Some(wire), &[]);
        let lane = s.lane(member).expect("the lane");
        let out = relay_request(lane.shape(), "application/json", carried.as_bytes());
        assert_eq!(
            String::from_utf8_lossy(&out.bytes),
            want,
            "{dialect}: only the governed members move"
        );
    }
}

/// Rehomes `crates/busbar-llm/src/engine/tests/multi_candidate_degrade_tests.rs::openai_to_anthropic_n_gt_1_is_forwarded_not_rejected`.

#[test]
fn an_openai_n_over_one_to_anthropic_is_forwarded_without_n() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("anthropic", "claude-3-5-sonnet", None, &[]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":16,"n":3}"#,
    );
    let r = build(HOST, &a, &h, &s, "p", "claude-3-5-sonnet").expect("forwarded, not refused");
    let v = far_json(&r);
    assert!(v.get("n").is_none(), "{v}");
    assert!(v.get("messages").is_some(), "{v}");
}

/// Rehomes `crates/busbar-llm/src/engine/tests/multi_candidate_degrade_tests.rs::gemini_ingress_to_openai_candidate_count_gt_1_is_forwarded_not_rejected`.

#[test]
fn a_gemini_candidate_count_over_one_to_openai_is_forwarded() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("openai", "gpt-4o", None, &[]);
    let a = arrived(
        "/v1beta/models/gpt-4o:generateContent",
        &h,
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"candidateCount":2}}"#,
    );
    let r = build(HOST, &a, &h, &s, "p", "gpt-4o").expect("forwarded, not refused");
    assert!(far_json(&r).get("messages").is_some());
}

/// Rehomes `crates/busbar-llm/src/engine/tests/multi_candidate_degrade_tests.rs::openai_to_openai_n_gt_1_is_preserved_verbatim`.

#[test]
fn a_same_dialect_openai_n_over_one_is_relayed_unchanged() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("openai", "gpt-4o", None, &[]);
    let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"n":3}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let r = build(HOST, &a, &h, &s, "p", "gpt-4o").expect("not refused");
    assert_eq!(far_json(&r).get("n"), Some(&json!(3)));
    assert_eq!(String::from_utf8_lossy(&r.body), body);
}

/// Rehomes `crates/busbar-llm/src/engine/tests/multi_candidate_degrade_tests.rs::multi_input_embeddings_to_gemini_embeds_first_not_rejected`.

#[test]
fn multi_input_embeddings_to_gemini_embed_the_first() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("gemini", "text-embedding-004", None, &[]);
    let a = arrived(
        "/v1/embeddings",
        &h,
        r#"{"model":"text-embedding-3-small","input":["alpha","beta","gamma"]}"#,
    );
    assert_eq!(a.operation, OpVerb::EMBEDDINGS);
    let r = build(HOST, &a, &h, &s, "p", "text-embedding-004").expect("forwarded, not refused");
    assert_eq!(
        far_json(&r).pointer("/content/parts/0/text"),
        Some(&json!("alpha"))
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/multi_candidate_degrade_tests.rs::single_input_embeddings_to_gemini_is_allowed`.

#[test]
fn single_input_embeddings_to_gemini_are_written() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("gemini", "text-embedding-004", None, &[]);
    let a = arrived(
        "/v1/embeddings",
        &h,
        r#"{"model":"text-embedding-3-small","input":["alpha"]}"#,
    );
    assert!(build(HOST, &a, &h, &s, "p", "text-embedding-004").is_ok());
}

/// The seven family operations the legacy parity test walked, each with a label for the table.
const OPS: [(OpVerb, &str); 7] = [
    (OpVerb::CHAT, "chat"),
    (OpVerb::EMBEDDINGS, "embeddings"),
    (OpVerb::MODERATION, "moderation"),
    (OpVerb::IMAGE, "image"),
    (OpVerb::TRANSCRIPTION, "transcription"),
    (OpVerb::SPEECH, "speech"),
    (OpVerb::RERANK, "rerank"),
];

fn released_path(
    dialect: &str,
    op: &str,
    stream: bool,
    model: &str,
    path: Option<&str>,
    base: Option<&str>,
) -> String {
    if let Some(p) = path {
        return p.to_string();
    }
    match dialect {
        "openai" => match op {
            "chat" => "/v1/chat/completions",
            "embeddings" => "/v1/embeddings",
            "moderation" => "/v1/moderations",
            "image" => "/v1/images/generations",
            "transcription" => "/v1/audio/transcriptions",
            "speech" => "/v1/audio/speech",
            _ => "/v1/rerank",
        }
        .to_string(),
        "anthropic" => match base {
            Some(b) => {
                let verb = if stream {
                    "streamRawPredict"
                } else {
                    "rawPredict"
                };
                format!("{b}/{model}:{verb}")
            }
            None => "/v1/messages".to_string(),
        },
        "gemini" => {
            let b = base.unwrap_or("/v1beta/models");
            let action = match op {
                "embeddings" => "embedContent",
                "image" => "predict",
                "chat" | "transcription" | "speech" if stream => "streamGenerateContent",
                _ => "generateContent",
            };
            format!("{b}/{model}:{action}")
        }
        "cohere" => match op {
            "chat" => "/v2/chat",
            "rerank" => "/v2/rerank",
            _ => "/v2/embed",
        }
        .to_string(),
        "bedrock" => match (op, stream) {
            ("chat", true) => format!("/model/{model}/converse-stream"),
            ("chat", false) => format!("/model/{model}/converse"),
            _ => format!("/model/{model}/invoke"),
        },
        _ => "/v1/responses".to_string(),
    }
}

/// RFC 3986 percent-encoding of a path with `/` kept, as 1.5.5's `sign_and_wire_path_parts` did

fn released_wire(url_path: &str) -> (String, String) {
    fn encode(p: &str) -> String {
        p.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    let (path, query) = match url_path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url_path, None),
    };
    let wire_path = encode(path);
    let canonical = if wire_path == path {
        path.to_string()
    } else {
        encode(&wire_path)
    };
    let wire = match query {
        Some(q) => format!("{wire_path}?{q}"),
        None => wire_path,
    };
    (wire, canonical)
}

/// Rehomes `crates/busbar-llm/src/engine/tests/plane_attempt_parity_tests.rs::the_planes_attempt_path_is_the_engines_url_after_its_base`.

#[test]
fn every_dialect_operation_shape_and_stream_intent_reads_the_released_far_path() {
    let shapes: [(&str, Option<&str>, Option<&str>); 4] = [
        ("m-1", None, None),
        (
            "m-1",
            Some("/openai/deployments/d/chat?api-version=2024-10-21"),
            None,
        ),
        (
            "m-1",
            None,
            Some("/v1/projects/x/locations/y/publishers/google"),
        ),
        ("anthropic.claude-3:0", None, None),
    ];
    let mut checked = 0;
    for dialect in DIALECTS {
        for (model, path, base) in shapes {
            let lane = Lane {
                model: model.to_string(),
                provider: "p".to_string(),
                dialect,
                path: path.map(str::to_string),
                path_base: base.map(str::to_string),
                options: Vec::new(),
                upstream_model: None,
                default_max_tokens: None,
                context_max: None,
                reasoning: false,
                prompt_caching: false,
                caps: Default::default(),
                error_map: Default::default(),
                statics: &[],
                invoke: false,
            };
            for (op, label) in OPS {
                for stream in [false, true] {
                    let want = released_path(dialect, label, stream, model, path, base);
                    let have = upstream_path(&lane, op, stream).expect("the plane's path");
                    let case =
                        format!("{dialect} {label} stream {stream} {model} {path:?} {base:?}");
                    assert_eq!(have, want, "{case}");
                    assert_eq!(
                        wire_and_canonical_path(&have),
                        released_wire(&want),
                        "{case}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 6 * 4 * 7 * 2);
    assert_eq!(
        released_wire("/model/anthropic.claude-3:0/converse"),
        (
            "/model/anthropic.claude-3%3A0/converse".to_string(),
            "/model/anthropic.claude-3%253A0/converse".to_string()
        )
    );
    assert_eq!(
        released_wire("/openai/deployments/d/chat?api-version=2024-10-21"),
        (
            "/openai/deployments/d/chat?api-version=2024-10-21".to_string(),
            "/openai/deployments/d/chat".to_string()
        )
    );
    assert_eq!(
        released_path("gemini", "moderation", true, "m-1", None, None),
        "/v1beta/models/m-1:generateContent",
        "1.5.5: a gemini verb with no action of its own is generateContent whatever the stream"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/request_short_circuit_tests.rs::pristine_same_proto_is_byte_identical_body_model`.

#[test]
fn a_pristine_same_dialect_body_is_byte_identical_for_four_body_model_dialects() {
    let h = head(&[("content-type", "application/json")]);
    for (dialect, target, model, body) in [
        (
            "anthropic",
            "/v1/messages",
            "claude-3",
            r#"{"model":"claude-3","max_tokens":7,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        (
            "openai",
            "/v1/chat/completions",
            "gpt-4o",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"temperature":0.5}"#,
        ),
        (
            "cohere",
            "/v2/chat",
            "command-r",
            r#"{"model":"command-r","messages":[{"role":"user","content":"hi"}]}"#,
        ),
        (
            "responses",
            "/v1/responses",
            "gpt-4o",
            r#"{"model":"gpt-4o","input":"hi"}"#,
        ),
    ] {
        let s = one_lane(dialect, model, None, &[]);
        let a = arrived(target, &h, body);
        assert_eq!(a.dialect, dialect);
        let r = build(HOST, &a, &h, &s, "p", model).expect("built");
        assert!(r.pristine, "{dialect}");
        assert_eq!(String::from_utf8_lossy(&r.body), body, "{dialect}");
    }
}

/// Rehomes `crates/busbar-llm/src/engine/tests/request_short_circuit_tests.rs::claude_on_vertex_drops_model_and_injects_anthropic_version`.

#[test]
fn claude_on_vertex_drops_the_model_and_names_the_vertex_version() {
    let vbase = "/v1/projects/p/locations/us-central1/publishers/anthropic/models";
    let s = one_lane(
        "anthropic",
        "claude-3-5-sonnet",
        None,
        &[("path_base", vbase)],
    );
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/messages",
        &h,
        r#"{"model":"claude-3-5-sonnet","max_tokens":7,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let r = build(HOST, &a, &h, &s, "p", "claude-3-5-sonnet").expect("built");
    let v = far_json(&r);
    assert!(v.get("model").is_none(), "{v}");
    assert_eq!(
        v.get("anthropic_version").and_then(Value::as_str),
        Some("vertex-2023-10-16"),
        "{v}"
    );
    assert!(v.get("messages").is_some(), "{v}");
    assert_eq!(
        r.target,
        "/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude-3-5-sonnet%3ArawPredict"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/request_short_circuit_tests.rs::invalidator_2_stream_on_path_model_egress_forces_non_pristine`.

#[test]
fn a_stream_member_never_reaches_a_path_model_far_end() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("gemini", "url-model-x", None, &[]);
    let body = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"stream":true}"#;
    let a = arrived("/v1beta/models/url-model-x:generateContent", &h, body);
    let r = build(HOST, &a, &h, &s, "p", "url-model-x").expect("built");
    assert!(!r.pristine);
    assert_ne!(String::from_utf8_lossy(&r.body), body);
    assert!(
        far_json(&r).get("stream").is_none(),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/request_short_circuit_tests.rs::invalidator_4_same_proto_model_shim_strip_forces_non_pristine`.

#[test]
fn a_body_model_member_never_reaches_a_same_dialect_path_model_far_end() {
    let h = head(&[("content-type", "application/json")]);
    for (dialect, target, body) in [
        (
            "gemini",
            "/v1beta/models/url-model-x:generateContent",
            r#"{"model":"router-shim","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        ),
        (
            "bedrock",
            "/model/url-model-x/converse",
            r#"{"model":"router-shim","messages":[{"role":"user","content":[{"text":"hi"}]}]}"#,
        ),
    ] {
        let s = one_lane(dialect, "url-model-x", None, &[]);
        let a = arrived(target, &h, body);
        let r = build(HOST, &a, &h, &s, "p", "url-model-x").expect("built");
        assert!(!r.pristine, "{dialect}");
        assert_ne!(String::from_utf8_lossy(&r.body), body, "{dialect}");
        assert!(far_json(&r).get("model").is_none(), "{dialect}");
    }
}

/// Rehomes `crates/busbar-llm/src/engine/tests/request_short_circuit_tests.rs::same_proto_gemini_thought_signature_round_trips_verbatim`.

#[test]
fn a_same_dialect_gemini_thought_signature_relays_verbatim() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("gemini", "url-model-x", None, &[]);
    let body = r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"SF"}},"thoughtSignature":"a-real-opaque-signature-token"}]}]}"#;
    let a = arrived("/v1beta/models/url-model-x:generateContent", &h, body);
    let r = build(HOST, &a, &h, &s, "p", "url-model-x").expect("built");
    let text = String::from_utf8_lossy(&r.body);
    assert_eq!(text, body);
    assert!(text.contains("a-real-opaque-signature-token"));
    assert!(!text.contains("skip_thought_signature_validator"));
}

/// Rehomes `crates/busbar-llm/src/engine/tests/stop_sequence_cap_degrade_tests.rs::cohere_to_cohere_over_cap_stop_sequences_is_preserved_verbatim`.

#[test]
fn a_same_dialect_cohere_over_cap_stop_list_goes_out_whole() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("cohere", "command-r-plus", None, &[]);
    let body = r#"{"model":"command-r-plus","messages":[{"role":"user","content":"hi"}],"stop_sequences":["a","b","c","d","e","f"]}"#;
    let a = arrived("/v2/chat", &h, body);
    let r = build(HOST, &a, &h, &s, "p", "command-r-plus").expect("not refused");
    assert_eq!(
        far_json(&r).get("stop_sequences"),
        Some(&json!(["a", "b", "c", "d", "e", "f"]))
    );
    assert_eq!(String::from_utf8_lossy(&r.body), body);
}

#[test]
fn cohere_bounds_combined_foreign_controls_are_clamped_and_audited_once() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("cohere", "command-r-plus", None, &[]);
    let body = r#"{"model":"command-r-plus","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"top_p":1.0,"frequency_penalty":1.5,"presence_penalty":-0.5}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let cap = busbar_test_kit::warn_capture::WarnCapture::default();
    let r = tracing::subscriber::with_default(cap.clone(), || {
        build(HOST, &a, &h, &s, "p", "command-r-plus").expect("translated")
    });
    let wire = far_json(&r);
    assert_eq!(
        wire,
        json!({"model":"command-r-plus", "max_tokens":16,
        "messages":[{"role":"user", "content":"hi"}],
        "p":0.99, "frequency_penalty":1.0, "presence_penalty":0.0})
    );
    assert!(wire.get("top_p").is_none());
    assert_eq!(
        r.dropped_controls,
        ["top_p", "frequency_penalty", "presence_penalty"]
    );
    let warnings = cap.messages();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    for parameter in ["top_p", "frequency_penalty", "presence_penalty"] {
        let path = format!("path={parameter}");
        assert_eq!(
            warnings
                .iter()
                .filter(|message| message.split_whitespace().any(|field| field == path))
                .count(),
            1,
            "{warnings:?}"
        );
    }
}

#[test]
fn cohere_bounds_native_attempt_preserves_bytes_without_clamping_or_diagnostics() {
    let h = head(&[("content-type", "application/json")]);
    let s = one_lane("cohere", "command-r-plus", None, &[]);
    let body = r#"{ "model" : "command-r-plus", "messages" : [{ "role" : "user", "content" : "hi" }], "p" : 1.0, "frequency_penalty" : 1.5, "presence_penalty" : -0.5 }"#;
    let a = arrived("/v2/chat", &h, body);
    let cap = busbar_test_kit::warn_capture::WarnCapture::default();
    let r = tracing::subscriber::with_default(cap.clone(), || {
        build(HOST, &a, &h, &s, "p", "command-r-plus").expect("relayed")
    });
    assert_eq!(r.body, body.as_bytes());
    assert!(r.pristine);
    assert!(r.dropped_controls.is_empty(), "{:?}", r.dropped_controls);
    assert!(cap.messages().is_empty(), "{:?}", cap.messages());
}

/// Rehomes `crates/busbar-llm/src/engine/tests/translate_offload_tests.rs::huge_body_translates_via_offload_and_forwards`.

#[test]
fn a_huge_body_is_written_like_a_small_one() {
    let h = head(&[("content-type", "application/json")]);
    let big = "x".repeat(300 * 1024);
    for content in [big.as_str(), "hi"] {
        let body = serde_json::to_string(&json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 16,
        }))
        .expect("JSON");
        assert!(content.len() < 1024 || body.len() >= 128 * 1024);
        let a = arrived("/v1/chat/completions", &h, &body);
        for dialect in ["openai", "anthropic"] {
            let s = one_lane(dialect, "gpt-4o", None, &[]);
            let r = build(HOST, &a, &h, &s, "p", "gpt-4o")
                .unwrap_or_else(|e| panic!("{dialect} {}: {e:?}", content.len()));
            let v = far_json(&r);
            let carried = v
                .pointer("/messages/0/content")
                .map(|content_value| match content_value {
                    Value::String(text) => text.len(),
                    other => other
                        .pointer("/0/text")
                        .and_then(Value::as_str)
                        .map_or(0, str::len),
                })
                .unwrap_or(0);
            assert_eq!(
                carried,
                content.len(),
                "{dialect}: the whole turn is carried"
            );
        }
    }
}

/// Rehomes `crates/busbar-llm/src/engine/attempt/tests/assemble.rs::a_wrong_typed_stream_options_is_refused_before_any_send`.

#[test]
fn every_wrong_typed_stream_options_is_refused_before_any_send() {
    let h = head(&[("content-type", "application/json")]);
    for wrong in [r#""x""#, "1", "true", "[]"] {
        let body =
            format!(r#"{{"model":"gpt","stream":true,"stream_options":{wrong},"messages":[]}}"#);
        let a = arrived("/v1/chat/completions", &h, &body);
        let refused = build(HOST, &a, &h, &shaping(), "p", "gpt")
            .expect_err("a wrong-typed stream_options is refused, never forwarded");
        assert_eq!(refused.status, 400, "stream_options = {wrong}");
        assert_eq!(refused.envelope, "openai", "stream_options = {wrong}");
        assert_eq!(
            refused.kind, "invalid_request_error",
            "stream_options = {wrong}"
        );
        assert_eq!(
            refused.message, DETAIL_STREAM_OPTIONS_NOT_OBJECT,
            "stream_options = {wrong}"
        );
        let rendered = busbar_plane_llm::exchange::refuse::answered(HOST, &refused);
        assert_eq!(rendered.status, 400);
        let text = String::from_utf8_lossy(&rendered.body);
        assert!(
            text.contains("Invalid type for 'stream_options': expected an object."),
            "the refusal names the field: {text}"
        );
    }
}

/// Rehomes `crates/busbar-llm/src/engine/attempt/tests/assemble.rs::the_injector_answers_err_on_a_wrong_typed_stream_options`.

#[test]
fn the_injector_hands_the_callers_bytes_back_on_a_wrong_typed_stream_options() {
    let body: &[u8] = br#"{"stream":true,"stream_options":"x"}"#;
    assert_eq!(
        try_inject_stream_include_usage(body.to_vec()),
        Err(body.to_vec())
    );
}

fn count_of(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::injector_idempotent_when_stream_options_already_present`.

#[test]
fn an_existing_stream_options_is_set_in_place_never_duplicated() {
    let body = br#"{"model":"gpt-4o","stream":true,"stream_options":{"include_usage":false},"messages":[]}"#;
    let out = try_inject_stream_include_usage(body.to_vec()).expect("asked");
    let v: Value = busbar_plane_llm::codec::json::parse(&out).expect("still JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true))
    );
    assert_eq!(
        count_of(&out, br#""stream_options""#),
        1,
        "{}",
        String::from_utf8_lossy(&out)
    );

    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"gpt","stream":true,"stream_options":{"include_usage":false},"messages":[]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("built");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("still JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true))
    );
    assert_eq!(count_of(&r.body, br#""stream_options""#), 1);
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::injector_tolerates_leading_whitespace`.

#[test]
fn the_usage_ask_lands_behind_mixed_leading_whitespace() {
    let out = try_inject_stream_include_usage(b"  \n\t{\"model\":\"m\",\"stream\":true}".to_vec())
        .expect("asked");
    let v: Value = busbar_plane_llm::codec::json::parse(&out).expect("valid JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true))
    );
    assert_eq!(v.pointer("/model"), Some(&json!("m")));
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::injector_falls_back_on_empty_or_non_object`.

#[test]
fn an_empty_object_gains_the_ask_and_a_non_object_passes() {
    let out = try_inject_stream_include_usage(b"{}".to_vec()).expect("asked");
    let v: Value = busbar_plane_llm::codec::json::parse(&out).expect("valid JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true)),
        "{v}"
    );
    let out = try_inject_stream_include_usage(b"[1,2,3]".to_vec()).expect("unchanged");
    assert_eq!(out, b"[1,2,3]");
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::injector_does_not_overrun_an_all_whitespace_body`.

#[test]
fn an_all_whitespace_body_passes_the_injector_untouched() {
    let out = try_inject_stream_include_usage(b"   \n\t ".to_vec()).expect("unchanged");
    assert_eq!(out, b"   \n\t ");
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::upgrades_existing_stream_options_preserving_siblings`.

#[test]
fn an_existing_false_is_overwritten_and_its_siblings_kept() {
    let body = br#"{"model":"gpt-4o","stream":true,"stream_options":{"include_usage":false,"foo":1},"messages":[]}"#;
    let out = try_inject_stream_include_usage(body.to_vec()).expect("asked");
    let v: Value = busbar_plane_llm::codec::json::parse(&out).expect("valid JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true)),
        "{v}"
    );
    assert_eq!(v.pointer("/stream_options/foo"), Some(&json!(1)), "{v}");
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::keeps_existing_true`.

#[test]
fn a_caller_already_opted_in_stays_opted_in() {
    let out = try_inject_stream_include_usage(
        br#"{"stream":true,"stream_options":{"include_usage":true}}"#.to_vec(),
    )
    .expect("asked");
    let v: Value = busbar_plane_llm::codec::json::parse(&out).expect("valid JSON");
    assert_eq!(
        v.pointer("/stream_options/include_usage"),
        Some(&json!(true))
    );

    let h = head(&[("content-type", "application/json")]);
    let body =
        r#"{"model":"gpt","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#;
    let a = arrived("/v1/chat/completions", &h, body);
    let r = build(HOST, &a, &h, &shaping(), "p", "gpt").expect("an opted-in caller is not refused");
    assert_eq!(r.body, body.as_bytes(), "the caller's own bytes");
}

/// Rehomes `crates/busbar-llm/src/engine/engine_tests/inject_include_usage_tests.rs::an_existing_stream_options_is_edited_in_place`.

#[test]
fn the_usage_ask_edits_an_existing_stream_options_in_place_byte_for_byte() {
    for (input, want) in [
        (
            &br#"{"model":"gpt-4o", "stream":true,"stream_options":{"foo":1, "include_usage":false},"messages":[]}"#[..],
            &br#"{"model":"gpt-4o", "stream":true,"stream_options":{"foo":1, "include_usage":true},"messages":[]}"#[..],
        ),
        (
            &br#"{"stream":true,"stream_options":{ "foo":1 },"z":1}"#[..],
            &br#"{"stream":true,"stream_options":{"include_usage":true, "foo":1 },"z":1}"#[..],
        ),
        (
            &br#"{"stream":true,"stream_options":null,"z":1}"#[..],
            &br#"{"stream":true,"stream_options":{"include_usage":true},"z":1}"#[..],
        ),
    ] {
        let out = try_inject_stream_include_usage(input.to_vec()).expect("asked");
        assert_eq!(
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(want),
            "{}",
            String::from_utf8_lossy(input)
        );
    }
}

/// A CROSS-DIALECT REQUEST LIFTS THE SYSTEM TURN: an openai caller's `system` message, sent to an

#[test]
fn a_cross_dialect_request_lifts_the_system_turn_out_of_the_messages() {
    let h = head(&[("content-type", "application/json")]);
    let a = arrived(
        "/v1/chat/completions",
        &h,
        r#"{"model":"claude","messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"}]}"#,
    );
    let r = build(HOST, &a, &h, &shaping(), "p", "claude").expect("built");
    let v: Value = busbar_plane_llm::codec::json::parse(&r.body).expect("json");
    assert!(v.get("system").is_some(), "{v}");
    let messages = v["messages"].as_array().expect("messages");
    assert!(!messages.iter().any(|m| m["role"] == "system"), "{v}");
    assert!(messages.iter().any(|m| m["role"] == "user"), "{v}");
}

/// AN OMITTED `max_tokens` IS FILLED, AN EXPLICIT ONE KEPT: an openai caller that omits

#[test]
fn an_omitted_max_tokens_is_filled_and_an_explicit_one_is_kept() {
    let shaping = Shaping::from_settings(&json!({
        "providers": { "ant": { "protocol": "anthropic", "base_url": "https://anthropic.example" } },
        "models": {
            "bare": { "provider": "ant" },
            "defaulted": { "provider": "ant", "default_max_tokens": 1234 }
        },
        "pools": { "p": { "members": ["bare", "defaulted"] } }
    }))
    .expect("reads");
    let h = head(&[("content-type", "application/json")]);
    let sent = |model: &str, body: &str| -> Value {
        let a = arrived("/v1/chat/completions", &h, body);
        let r = build(HOST, &a, &h, &shaping, "p", model).expect("built");
        busbar_plane_llm::codec::json::parse(&r.body).expect("json")
    };
    let omitted = sent(
        "bare",
        r#"{"model":"bare","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        omitted["max_tokens"],
        u64::from(busbar_plane_llm::exchange::shaping::DEFAULT_MAX_TOKENS),
        "{omitted}"
    );
    let explicit = sent(
        "defaulted",
        r#"{"model":"defaulted","messages":[{"role":"user","content":"hi"}],"max_tokens":7}"#,
    );
    assert_eq!(explicit["max_tokens"], 7, "{explicit}");
}

#[test]
fn red_a_same_dialect_route_forwards_no_caller_credential() {
    let ungoverned = [
        ("Cookie", "session=CANARY-cookie"),
        ("Proxy-Authorization", "Basic CANARY-proxy-authorization"),
        ("x-amz-security-token", "CANARY-x-amz-security-token"),
        ("X-Client-Trace", "kept"),
    ];
    for (target, body, model, own) in [
        (
            "/v1/messages",
            r#"{"model":"claude","max_tokens":5,"messages":[]}"#,
            "claude",
            ("x-api-key", "CANARY-x-api-key"),
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"gpt","messages":[]}"#,
            "gpt",
            ("authorization", "Bearer CANARY-authorization"),
        ),
    ] {
        let mut pairs = vec![("content-type", "application/json"), own];
        pairs.extend(ungoverned);
        let h = head(&pairs);
        let a = arrived(target, &h, body);
        let r = build(HOST, &a, &h, &shaping(), "p", model).expect("built");
        for (name, value) in &r.fields {
            assert!(
                !value.windows(6).any(|w| w == b"CANARY"),
                "{target}: the caller's `{name}` reached the far end"
            );
        }
        assert_eq!(
            field(&r, "x-client-trace"),
            [b"kept".as_slice()],
            "{target}"
        );
    }
}
