// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PROVIDER OPTIONS (`busbar_plane_llm::options`): a provider's block takes busbar's keys and
//! the names its dialect's far end accepts, nothing else. A set option reaches the wire. An unknown
//! name, a credential, a hop-by-hop field or a value of the wrong type fails the load. A
//! same-dialect caller's own value beats an operator default, and a forced option beats the caller.

mod common;

use common::TestHost;
use serde_json::{json, Value};

use busbar_plane_llm::dialect::DIALECTS;
use busbar_plane_llm::exchange::arrive::{arrive, Arrived};
use busbar_plane_llm::exchange::attempt::{build, governed, FarRequest};
use busbar_plane_llm::exchange::shaping::Shaping;
use busbar_plane_llm::options::{catalogue, Place, Precedence};

const HOST: &dyn busbar_contract::codec::CodecHost = &TestHost;

/// One provider `far` with `block`, serving model `m` (its own `protocol` when given) in pool `p`.
fn settings(block: Value, model_protocol: Option<&str>) -> Value {
    let mut model = json!({ "provider": "far" });
    if let Some(p) = model_protocol {
        model["protocol"] = json!(p);
    }
    json!({
        "providers": { "far": block },
        "models": { "m": model },
        "pools": { "p": { "members": ["m"] } }
    })
}

fn load(block: Value) -> Result<Shaping, String> {
    Shaping::from_settings(&settings(block, None))
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

const OPENAI_BODY: &str = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
const ANTHROPIC_BODY: &str =
    r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":10}"#;

fn send(s: &Shaping, target: &str, h: &[(&'static str, &'static str)], body: &str) -> FarRequest {
    let h = head(h);
    let a = arrived(target, &h, body);
    build(HOST, &a, &h, s, "p", "m").expect("built")
}

/// A set option reaches the wire, on every dialect's placement: head fields under their wire name,
/// whatever case the operator wrote it in, and a query parameter on the target.
#[test]
fn a_set_option_reaches_the_wire() {
    let s = load(json!({
        "protocol": "openai",
        "base_url": "https://x.example",
        "openai-organization": "org-abc",
        "OpenAI-Project": "proj_1",
        "api-version": "2024-10-21"
    }))
    .expect("reads");
    let r = send(
        &s,
        "/v1/chat/completions",
        &[("content-type", "application/json")],
        OPENAI_BODY,
    );
    assert_eq!(field(&r, "openai-organization"), [b"org-abc".as_slice()]);
    assert_eq!(field(&r, "openai-project"), [b"proj_1".as_slice()]);
    assert_eq!(r.target, "/v1/chat/completions?api-version=2024-10-21");

    let s =
        load(json!({ "protocol": "gemini", "x-goog-user-project": "my-gcp-proj" })).expect("reads");
    let r = send(
        &s,
        "/v1/chat/completions",
        &[("content-type", "application/json")],
        OPENAI_BODY,
    );
    assert_eq!(
        field(&r, "x-goog-user-project"),
        [b"my-gcp-proj".as_slice()]
    );

    let s = load(json!({
        "protocol": "anthropic",
        "anthropic-beta": ["prompt-caching-2024-07-31", "output-128k-2025-02-19"],
        "anthropic-version": "2023-01-01"
    }))
    .expect("reads");
    let r = send(
        &s,
        "/v1/chat/completions",
        &[("content-type", "application/json")],
        OPENAI_BODY,
    );
    assert_eq!(
        field(&r, "anthropic-beta"),
        [b"prompt-caching-2024-07-31,output-128k-2025-02-19".as_slice()]
    );
    assert_eq!(
        field(&r, "anthropic-version"),
        [b"2023-01-01".as_slice()],
        "the operator's version replaces the declared default, once"
    );
}

/// A provider that sets no option writes exactly what it wrote before options existed: the
/// anthropic dialect's declared version, and no other field or parameter.
#[test]
fn a_provider_that_sets_nothing_sends_nothing_extra() {
    let s = load(json!({ "protocol": "anthropic" })).expect("reads");
    assert!(s.lane("m").expect("lane").options.is_empty());
    let r = send(
        &s,
        "/v1/messages",
        &[("content-type", "application/json")],
        ANTHROPIC_BODY,
    );
    assert_eq!(field(&r, "anthropic-version"), [b"2023-06-01".as_slice()]);
    assert!(field(&r, "anthropic-beta").is_empty());
    assert_eq!(r.target, "/v1/messages");
}

/// A name on no list fails the load, naming the key and listing what the dialect accepts in the
/// provider's documented spelling. A name valid on one dialect is unknown on another.
#[test]
fn an_unknown_option_is_refused_with_the_dialects_list() {
    let e = load(json!({ "protocol": "openai", "hello": 1 })).expect_err("refused");
    assert_eq!(
        e,
        "providers.far: unknown key `hello`: it is not a busbar provider key and not an option of \
         the provider's dialect; the openai dialect accepts `OpenAI-Organization`, \
         `OpenAI-Project`, `api-version`"
    );
    let e = load(json!({ "protocol": "gemini", "OpenAI-Organization": "org-abc" }))
        .expect_err("refused");
    assert!(e.contains("unknown key `OpenAI-Organization`"), "{e}");
    assert!(
        e.contains("the gemini dialect accepts `X-Goog-User-Project`, `X-Goog-Quota-User`"),
        "{e}"
    );
    // The retired friendly names are not aliases.
    let e = load(json!({ "protocol": "openai", "organization": "org-abc" })).expect_err("refused");
    assert!(e.contains("unknown key `organization`"), "{e}");
    // A query parameter's name matches exactly.
    let e =
        load(json!({ "protocol": "openai", "API-Version": "2024-10-21" })).expect_err("refused");
    assert!(e.contains("unknown key `API-Version`"), "{e}");
}

/// A credential or auth field is on no list, and naming one is refused with that reason.
#[test]
fn an_auth_field_is_refused() {
    for (dialect, key) in [
        ("openai", "Authorization"),
        ("openai", "api-key"),
        ("anthropic", "x-api-key"),
        ("gemini", "x-goog-api-key"),
        ("gemini", "key"),
        ("bedrock", "X-Amz-Security-Token"),
        ("bedrock", "x-amz-date"),
        ("cohere", "Proxy-Authorization"),
    ] {
        let e = load(json!({ "protocol": dialect, key: "secret" })).expect_err("refused");
        assert_eq!(
            e,
            format!(
                "providers.far: `{key}` is a credential or auth field and cannot be set \
                 statically; busbar presents the provider's `api_key` under its `auth` style"
            ),
            "{dialect}"
        );
        assert!(!e.contains("secret"), "the value is never echoed");
    }
}

/// A hop-by-hop or connection-level field is refused with that reason.
#[test]
fn a_hop_by_hop_field_is_refused() {
    for key in [
        "Connection",
        "transfer-encoding",
        "Host",
        "content-length",
        "Upgrade",
    ] {
        let e = load(json!({ "protocol": "openai", key: "x" })).expect_err("refused");
        assert!(e.contains(&format!("`{key}` is a hop-by-hop")), "{e}");
    }
}

/// A value of the wrong type is refused, saying what the name takes and echoing the name in the
/// provider's spelling.
#[test]
fn a_wrong_type_is_refused_with_what_the_name_takes() {
    let e = load(json!({ "protocol": "gemini", "x-server-timeout": "soon" })).expect_err("refused");
    assert_eq!(
        e,
        "providers.far.X-Server-Timeout: takes a whole number of seconds, got a string not of \
         that form"
    );
    let e = load(json!({ "protocol": "openai", "OpenAI-Organization": 7 })).expect_err("refused");
    assert!(
        e.starts_with("providers.far.OpenAI-Organization: takes one token"),
        "{e}"
    );
    let e = load(json!({ "protocol": "anthropic", "anthropic-version": "2099-01-01" }))
        .expect_err("refused");
    assert!(e.contains("takes one of `2023-06-01`, `2023-01-01`"), "{e}");
    let e = load(json!({ "protocol": "openai", "OpenAI-Project": "a", "openai-project": "b" }))
        .expect_err("refused");
    assert!(e.contains("set twice"), "{e}");
}

/// PRECEDENCE. A same-dialect caller's own head field and query parameter beat the operator's
/// default. A forced option (a tenant selector) beats the caller's, which never goes out.
#[test]
fn per_request_beats_static_unless_forced() {
    let s = load(json!({
        "protocol": "anthropic",
        "anthropic-beta": "operator-beta-1"
    }))
    .expect("reads");
    let r = send(
        &s,
        "/v1/messages",
        &[
            ("content-type", "application/json"),
            ("anthropic-beta", "caller-beta-2"),
        ],
        ANTHROPIC_BODY,
    );
    assert_eq!(field(&r, "anthropic-beta"), [b"caller-beta-2".as_slice()]);
    // A caller of another dialect forwards no field: the operator's default goes out.
    let r = send(
        &s,
        "/v1/chat/completions",
        &[("content-type", "application/json")],
        OPENAI_BODY,
    );
    assert_eq!(field(&r, "anthropic-beta"), [b"operator-beta-1".as_slice()]);

    let s = load(json!({
        "protocol": "openai",
        "OpenAI-Organization": "org-operator",
        "api-version": "2024-10-21"
    }))
    .expect("reads");
    let h = [
        ("content-type", "application/json"),
        ("openai-organization", "org-caller"),
    ];
    let hh = head(&h);
    let a = arrive(
        "POST",
        "/v1/chat/completions?api-version=2025-01-01-preview",
        &hh,
        OPENAI_BODY.as_bytes(),
        &(),
    )
    .expect("arrives");
    let r = build(HOST, &a, &hh, &s, "p", "m").expect("built");
    assert_eq!(
        r.target, "/v1/chat/completions?api-version=2025-01-01-preview",
        "the caller's own query value wins over the operator's default"
    );
    assert_eq!(
        field(&r, "openai-organization"),
        [b"org-operator".as_slice()],
        "a forced option goes out and the caller's never does"
    );
}

/// Every forced option is a field its dialect governs, so a caller's value of it never passes.
#[test]
fn every_forced_option_is_governed() {
    for d in DIALECTS {
        for o in catalogue(d.name) {
            if o.precedence == Precedence::Forced {
                assert_eq!(o.place, Place::Header, "{}", o.name);
                assert!(
                    d.governed_headers
                        .iter()
                        .any(|g| g.eq_ignore_ascii_case(o.name)),
                    "{} {}",
                    d.name,
                    o.name
                );
                assert!(governed(o.name));
            }
        }
    }
}

/// No list names a credential the dialect governs, and every dialect the plane speaks has a list.
#[test]
fn no_list_names_a_credential() {
    for d in DIALECTS {
        for o in catalogue(d.name) {
            let governed_cred = d
                .governed_headers
                .iter()
                .any(|g| g.eq_ignore_ascii_case(o.name))
                && o.precedence != Precedence::Forced;
            assert!(!governed_cred, "{} lists the credential {}", d.name, o.name);
            assert!(!d.governed_query.contains(&o.name), "{} {}", d.name, o.name);
        }
        assert!(!catalogue(d.name).is_empty(), "{} has a list", d.name);
    }
}

/// Bedrock's `InvokeModel` headers reach a model served over the invoke envelope, and not a
/// Converse model, which takes the same settings in its body.
#[test]
fn bedrock_invoke_headers_reach_only_invoke_models() {
    let block = json!({
        "protocol": "bedrock",
        "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com",
        "X-Amzn-Bedrock-Trace": "ENABLED"
    });
    let invoke =
        Shaping::from_settings(&settings(block.clone(), Some("bedrock-invoke"))).expect("reads");
    assert_eq!(invoke.lane("m").expect("lane").options.len(), 1);
    let r = send(
        &invoke,
        "/v1/messages",
        &[("content-type", "application/json")],
        ANTHROPIC_BODY,
    );
    assert_eq!(field(&r, "x-amzn-bedrock-trace"), [b"ENABLED".as_slice()]);

    let converse = Shaping::from_settings(&settings(block, None)).expect("reads");
    assert!(converse.lane("m").expect("lane").options.is_empty());
}
