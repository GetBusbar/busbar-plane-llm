// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The far end's answer, sans I/O: a far-end error judged, a whole answer translated, a relayed

use std::collections::HashMap;

use busbar_contract::operation::OpVerb;
use busbar_contract::upstream::{Disposition, RawUpstreamError, StatusClass};
use busbar_plane_llm::exchange::reply::failure::{
    auth_failure, judge, normalize, relay_verbatim, FarError,
};
use busbar_plane_llm::exchange::reply::wire::head_field;
use serde_json::Value;

/// The codec host these tests hand the plane: a FIXED entropy source and the system clock in whole

struct TestHost;

impl busbar_contract::codec::CodecHost for TestHost {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }

    /// A FIXED wall clock: a relay stamps `created` from it, and the piece-boundary test compares two

    fn wall_clock_now(&self) -> Option<u64> {
        Some(1_700_000_000)
    }
}

/// The handle every codec call here is handed.
const HOST: &dyn busbar_contract::codec::CodecHost = &TestHost;

const SIX: [&str; 6] = [
    "anthropic",
    "openai",
    "gemini",
    "bedrock",
    "responses",
    "cohere",
];

fn raw(status: u16, code: Option<&str>, ty: Option<&str>) -> RawUpstreamError {
    let mut r = RawUpstreamError::from_status(status);
    r.provider_code = code.map(str::to_string);
    r.structured_type = ty.map(str::to_string);
    r
}

fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn field<'a>(fields: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_slice())
}

/// A dialect may declare a relayed head field the way its vendor spells it; the caller reads the

#[test]
fn a_declared_head_field_name_is_lowered_and_an_illegal_one_dropped() {
    let (name, value) = head_field("x-amzn-RequestId", b"req-1").expect("a legal name");
    assert_eq!(name, "x-amzn-requestid");
    assert_eq!(value, b"req-1");
    assert_eq!(
        head_field("x-amzn-requestid", b"req-1").map(|f| f.0),
        Some(name)
    );
    for illegal in ["x amzn requestid", "x-amzn-\u{1f600}", ""] {
        assert!(head_field(illegal, b"v").is_none(), "{illegal:?}");
    }
}

/// The class of a far-end error: a mapped code, the built-in context-length code on a request-size

#[test]
fn a_far_end_error_is_placed_in_its_class() {
    let em = map(&[
        ("1302", "rate_limit"),
        ("quota", "billing"),
        ("ctx", "context_length"),
        ("typo", "rate_limt"),
        ("overloaded_error", "overloaded"),
    ]);
    let cases: &[(u16, Option<&str>, Option<&str>, StatusClass)] = &[
        (400, Some("1302"), None, StatusClass::RateLimit),
        (402, Some("quota"), None, StatusClass::Billing),
        (400, Some("ctx"), None, StatusClass::ContextLength),
        (503, Some("ctx"), None, StatusClass::ServerError),
        (
            400,
            Some("context_length_exceeded"),
            None,
            StatusClass::ContextLength,
        ),
        (
            413,
            Some("context_length_exceeded"),
            None,
            StatusClass::ContextLength,
        ),
        (
            401,
            Some("context_length_exceeded"),
            None,
            StatusClass::Auth,
        ),
        (400, Some("typo"), None, StatusClass::ClientError),
        (500, None, Some("overloaded_error"), StatusClass::Overloaded),
        (503, None, Some("ctx"), StatusClass::ServerError),
        (401, None, None, StatusClass::Auth),
        (403, None, None, StatusClass::Auth),
        (429, None, None, StatusClass::RateLimit),
        (408, None, None, StatusClass::Timeout),
        (529, None, None, StatusClass::Overloaded),
        (502, None, None, StatusClass::ServerError),
        (404, None, None, StatusClass::ClientError),
        (302, None, None, StatusClass::ClientError),
    ];
    for &(status, code, ty, want) in cases {
        let r = raw(status, code, ty);
        assert_eq!(normalize(status, &r, &em).class, want, "{r:?}");
    }
    let sig = normalize(400, &raw(400, Some("1302"), None), &em);
    assert_eq!(sig.provider_signal.as_deref(), Some("1302"));

    let sig = normalize(400, &raw(400, None, Some("overloaded_error")), &em);
    assert_eq!(sig.class, StatusClass::Overloaded);
    assert_eq!(sig.provider_signal.as_deref(), Some("overloaded_error"));

    let sig = normalize(
        400,
        &raw(400, Some("context_length_exceeded"), None),
        &HashMap::new(),
    );
    assert_eq!(sig.class, StatusClass::ContextLength);
    assert_eq!(
        sig.provider_signal.as_deref(),
        Some("context_length_exceeded")
    );
}

const JSON_HEAD: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];

/// A far-end error judged, for every dialect: a caller's own credential failing is relayed

#[test]
fn a_far_end_error_is_judged_for_every_pair() {
    let head = JSON_HEAD;
    let body = br#"{"error":{"message":"bad things","type":"invalid_request_error"}}"#;
    for ingress in SIX {
        for egress in SIX {
            let far = |status| FarError { status, head, body };
            let j = judge(
                HOST,
                ingress,
                egress,
                OpVerb::CHAT,
                &HashMap::new(),
                true,
                &far(401),
            );
            assert_eq!(j.signal, None);
            assert_eq!(j.disposition, Disposition::ClientFault);
            assert_eq!(j.answer.status, 401);

            let j = judge(
                HOST,
                ingress,
                egress,
                OpVerb::CHAT,
                &HashMap::new(),
                false,
                &far(400),
            );
            assert_eq!(
                j.disposition,
                Disposition::ClientFault,
                "{ingress}<-{egress}"
            );
            if ingress == egress {
                assert_eq!(j.answer.body, body, "{ingress}: verbatim");
                assert_eq!(
                    field(&j.answer.fields, "content-type"),
                    Some(b"application/json".as_slice())
                );
            } else {
                let v: Value = serde_json::from_slice(&j.answer.body).expect("JSON");
                assert!(
                    v.to_string().contains("bad things"),
                    "{ingress}<-{egress}: {v}"
                );
            }

            let j = judge(
                HOST,
                ingress,
                egress,
                OpVerb::CHAT,
                &HashMap::new(),
                false,
                &far(401),
            );
            assert_eq!(j.disposition, Disposition::HardDown);
            assert_eq!(j.answer, auth_failure(HOST, ingress), "{ingress}<-{egress}");

            let j = judge(
                HOST,
                ingress,
                egress,
                OpVerb::CHAT,
                &HashMap::new(),
                false,
                &far(503),
            );
            assert_eq!(j.disposition, Disposition::TransientUpstream);
            assert_eq!(j.answer.status, 503);
        }
    }
}

/// A caller that relays the far end's `x-amzn-*` fields reads them verbatim on a same-dialect

#[test]
fn a_verbatim_relay_carries_the_native_head_fields() {
    let head: &[(&[u8], &[u8])] = &[
        (b"Content-Type", b"application/json"),
        (b"x-amzn-requestid", b"amzn-1"),
        (b"x-amzn-errortype", b"ValidationException"),
        (b"request-id", b"req_1"),
    ];
    let far = FarError {
        status: 400,
        head,
        body: b"{}",
    };
    let bedrock = relay_verbatim("bedrock", &far);
    assert_eq!(
        field(&bedrock.fields, "x-amzn-requestid"),
        Some(b"amzn-1".as_slice())
    );
    assert_eq!(
        field(&bedrock.fields, "x-amzn-errortype"),
        Some(b"ValidationException".as_slice())
    );
    let anthropic = relay_verbatim("anthropic", &far);
    assert_eq!(
        field(&anthropic.fields, "request-id"),
        Some(b"req_1".as_slice())
    );
    let openai = relay_verbatim("openai", &far);
    assert_eq!(
        openai.fields.len(),
        4,
        "every far field: {:?}",
        openai.fields
    );
}

/// BUSBAR IS INVISIBLE ON A SAME-DIALECT ANSWER: every far head field reaches the caller in order,

#[test]
fn a_verbatim_relay_carries_every_far_field_but_the_governed_ones() {
    let head: &[(&[u8], &[u8])] = &[
        (b"content-type", b"application/json"),
        (b"x-ratelimit-remaining-tokens", b"99"),
        (b"Retry-After", b"3"),
        (b"openai-organization", b"org-operator"),
        (b"openai-project", b"proj-operator"),
        (b"Set-Cookie", b"__cf_bm=operator-connection; path=/"),
        (b"anthropic-ratelimit-tokens-remaining", b"5"),
    ];
    let far = FarError {
        status: 429,
        head,
        body: b"{}",
    };
    let openai = relay_verbatim("openai", &far);
    let names: Vec<&str> = openai.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["content-type", "retry-after"]);
    let anthropic = relay_verbatim("anthropic", &far);
    assert!(
        !anthropic.fields.iter().any(|(n, _)| n == "request-id"),
        "a plane never invents a head field: {:?}",
        anthropic.fields
    );
    assert!(anthropic
        .fields
        .iter()
        .any(|(n, _)| n == "openai-organization"));
    for dialect in [
        "openai",
        "anthropic",
        "cohere",
        "gemini",
        "bedrock",
        "responses",
    ] {
        let relayed = relay_verbatim(dialect, &far);
        for (name, _) in &relayed.fields {
            assert!(
                !name.eq_ignore_ascii_case("set-cookie") && !name.contains("ratelimit"),
                "{dialect} relays the operator's account state `{name}`: {:?}",
                relayed.fields
            );
        }
    }
}

use busbar_plane_llm::exchange::reply::whole::{self, WholeCtx, WholeEnd};

/// The letter the codec's golden corpus names each dialect by.
fn letter(dialect: &str) -> char {
    match dialect {
        "anthropic" => 'a',
        "openai" => 'o',
        "gemini" => 'g',
        "bedrock" => 'b',
        "responses" => 'r',
        "cohere" => 'c',
        other => panic!("{other}"),
    }
}

/// Every native answer of `dialect` the codec's golden corpus holds: each `resp_X2Y_*.json` is a

pub fn native_answers(dialect: &str) -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/codec/tests/proto/golden");
    let infix = format!("2{}_", letter(dialect));
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("the golden corpus is readable")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("resp_") && n.ends_with(".json") && n[6..].starts_with(&infix))
        .map(|n| {
            let bytes = std::fs::read(dir.join(&n)).expect("a golden is readable");
            (n, bytes)
        })
        .collect();
    out.sort();
    out
}

fn ctx<'a>(
    ingress: &'a str,
    egress: &'a str,
    wants_stream: bool,
    json_array: bool,
) -> WholeCtx<'a> {
    WholeCtx {
        ingress,
        egress,
        operation: OpVerb::CHAT,
        model: "m-1",
        wants_stream,
        json_array,
        request: None,
        now_s: 1_752_000_000,
        elapsed_ms: Some(12),
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }
}

/// Every far end's native answer, for a caller of every other dialect, is delivered in the

#[test]
fn a_whole_answer_is_delivered_in_the_callers_dialect() {
    let mut checked = 0;
    for egress in SIX {
        let answers = native_answers(egress);
        assert!(
            !answers.is_empty(),
            "{egress}: no native answer in the corpus"
        );
        for (name, body) in &answers {
            for ingress in SIX.iter().copied().filter(|i| *i != egress) {
                let w = whole::translate(HOST, &ctx(ingress, egress, false, false), 200, body);
                assert_eq!(
                    w.end,
                    WholeEnd::Delivered,
                    "{ingress}<-{egress} {name}: {:?}",
                    w.refused
                );
                assert_eq!(w.answer.status, 200);
                assert_eq!(
                    field(&w.answer.fields, "content-type"),
                    Some(b"application/json".as_slice()),
                    "{ingress}<-{egress} {name}"
                );
                let _: Value = serde_json::from_slice(&w.answer.body).expect("a JSON answer");
                assert!(w.usage.is_some(), "{ingress}<-{egress} {name}: usage");
                checked += 1;
            }
        }
    }
    assert!(checked >= 6 * 5, "{checked}");
}

/// A generation the far end reports as failed is a 502 that still carries the usage it reported.
#[test]
fn a_failed_generation_is_a_502_that_keeps_its_usage() {
    let body = br#"{"id":"c-1","finish_reason":"ERROR","message":{"role":"assistant","content":[{"type":"text","text":"x"}]},"usage":{"tokens":{"input_tokens":10,"output_tokens":5}}}"#;
    let w = whole::translate(HOST, &ctx("openai", "cohere", false, false), 200, body);
    assert_eq!(w.end, WholeEnd::FailedGeneration);
    assert_eq!(w.answer.status, 502);
    assert!(w.usage.is_some());
}

/// An answer the caller's dialect cannot be written from is a 500 in the caller's envelope; the

#[test]
fn an_untranslatable_answer_is_the_callers_500() {
    for ingress in SIX {
        let w = whole::translate(
            HOST,
            &ctx(ingress, "anthropic", false, false),
            200,
            b"not json at all",
        );
        assert_eq!(w.end, WholeEnd::NotTranslatable, "{ingress}");
        assert_eq!(w.answer.status, 500);
        assert!(w.usage.is_none());
        assert_eq!(whole::over_cap(HOST, ingress).answer.status, 500);
        assert_eq!(whole::over_cap(HOST, ingress).end, WholeEnd::OverCap);
        assert_eq!(whole::cut(HOST, ingress).answer.status, 502);
        assert_eq!(whole::cut(HOST, ingress).end, WholeEnd::Cut);
    }
}

/// A caller that asked for a JSON-array stream from a far end that answered one body reads a

#[test]
fn a_json_array_caller_reads_a_one_element_array() {
    let (_, body) = native_answers("openai")
        .into_iter()
        .next()
        .expect("an openai answer");
    let w = whole::translate(HOST, &ctx("gemini", "openai", true, true), 200, &body);
    assert_eq!(w.end, WholeEnd::Delivered);
    let v: Value = serde_json::from_slice(&w.answer.body).expect("JSON");
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(w.answer.fields.len(), 1, "{:?}", w.answer.fields);
}

use busbar_plane_llm::exchange::reply::relay::{self, ContentType, Fed, Relay, RelayCtx};

fn chat_handler(dialect: &str) -> &'static dyn busbar_contract::codec::OperationHandler {
    busbar_plane_llm::codec::DECLS
        .iter()
        .find(|d| d.name == dialect)
        .and_then(|d| d.handler)
        .and_then(|rh| rh.operation_handler(OpVerb::CHAT))
        .expect("every dialect serves chat")
}

fn relay_ctx<'a>(ingress: &'a str, egress: &'a str, far_is_stream: bool) -> RelayCtx<'a> {
    RelayCtx {
        ingress,
        egress,
        far_is_stream,
        json_array: false,
        client_include_usage: false,
        request: None,
        handler: chat_handler(ingress),
        meter: true,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }
}

/// An OpenAI chat stream: two text deltas, the stop, the usage chunk and the terminator.
fn openai_stream() -> Vec<u8> {
    let chunk = |delta: &str, finish: &str| {
        format!(
            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m-1\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
        )
    };
    let mut s = String::new();
    s += &chunk(r#"{"role":"assistant","content":"Hel"}"#, "null");
    s += &chunk(r#"{"content":"lo"}"#, "null");
    s += &chunk("{}", r#""stop""#);
    s += "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n";
    s += "data: [DONE]\n\n";
    s.into_bytes()
}

/// Every piece a relay answers, concatenated, for `far` fed in pieces of `step` bytes.
fn relay_all(r: &mut Relay, far: &[u8], step: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in far.chunks(step) {
        if let (Fed::Bytes(b), _) = r.feed(HOST, piece) {
            out.extend_from_slice(&b);
        }
    }
    out.extend(r.end(HOST).bytes);
    out
}

/// A far end's native stream of `dialect`: the OpenAI stream above as the translator writes it for

fn native_stream(dialect: &str) -> Vec<u8> {
    let mut ctx = relay_ctx(dialect, "openai", true);
    ctx.client_include_usage = true;
    let mut r = Relay::new(ctx);
    relay_all(&mut r, &openai_stream(), usize::MAX)
}

/// A same-dialect whole body relays untouched (the far end's own bytes, borrowed) and its usage is

#[test]
fn a_same_dialect_whole_body_relays_untouched_and_meters() {
    for dialect in SIX {
        let (_, body) = native_answers(dialect)
            .into_iter()
            .next()
            .expect("an answer");
        let mut r = Relay::new(relay_ctx(dialect, dialect, false));
        let mut out = Vec::new();
        for piece in body.chunks(9) {
            match r.feed(HOST, piece) {
                (Fed::Bytes(std::borrow::Cow::Borrowed(b)), _) => out.extend_from_slice(b),
                other => panic!("{dialect}: {other:?}"),
            }
        }
        let end = r.end(HOST);
        assert_eq!(out, body, "{dialect}");
        assert!(end.bytes.is_empty() && !end.failed, "{dialect}");
        assert!(end.usage.is_some(), "{dialect}: usage");
        assert_eq!(
            relay::content_type(dialect, dialect, false, false),
            ContentType::Far
        );
    }
}

/// `json` with a member busbar has never heard of spliced in after its first opening brace.
fn with_unknown_member(json: &[u8]) -> Vec<u8> {
    let brace = json.iter().position(|b| *b == b'{').expect("an object");
    let mut out = json[..=brace].to_vec();
    out.extend_from_slice(br#""zz_never_heard_of":{"b":1,"a":[1.50, "x"]},"#);
    out.extend_from_slice(&json[brace + 1..]);
    out
}

/// SAME-DIALECT IDENTITY, the answer side (DIALECT FIDELITY; DIALECT-FIDELITY-DESIGN F4): in

#[test]
fn a_same_dialect_answer_carrying_an_unknown_member_reaches_the_caller_byte_identical() {
    for dialect in SIX {
        let (_, body) = native_answers(dialect)
            .into_iter()
            .next()
            .expect("an answer");
        let body = with_unknown_member(&body);
        let mut r = Relay::new(relay_ctx(dialect, dialect, false));
        let out = relay_all(&mut r, &body, 9);
        assert_eq!(out, body, "{dialect}: buffered");

        let native = native_stream(dialect);
        let far = if dialect == "bedrock" {
            let mut far = busbar_plane_llm::codec::eventstream::encode_frame(
                "zzNeverHeardOf",
                br#"{"zz":{"b":1,"a":[1.50]}}"#,
            );
            far.extend_from_slice(&native);
            far
        } else {
            let at = native
                .windows(5)
                .position(|w| w == b"data:")
                .expect("a data line");
            let mut far = native[..at].to_vec();
            far.extend_from_slice(&with_unknown_member(&native[at..]));
            far
        };
        let mut ctx = relay_ctx(dialect, dialect, true);
        ctx.client_include_usage = true;
        let mut r = Relay::new(ctx);
        let out = relay_all(&mut r, &far, 7);
        assert_eq!(out, far, "{dialect}: stream");
        let mut ctx = relay_ctx(dialect, dialect, true);
        ctx.client_include_usage = true;
        let mut r = Relay::new(ctx);
        for piece in far.chunks(7) {
            let _ = r.feed(HOST, piece);
        }
        assert!(
            r.end(HOST).usage.is_some(),
            "{dialect}: the stream is metered"
        );
    }
}

/// A stream from a far end of every dialect reaches a caller of every dialect whole, whatever the

#[test]
fn a_stream_relays_for_every_pair_whatever_the_piece_boundaries() {
    let mut checked = 0;
    for egress in SIX {
        let far = native_stream(egress);
        assert!(!far.is_empty(), "{egress}");
        for ingress in SIX {
            let whole = relay_all(
                &mut Relay::new(relay_ctx(ingress, egress, true)),
                &far,
                usize::MAX,
            );
            let mut r = Relay::new(relay_ctx(ingress, egress, true));
            let pieces = relay_all(&mut r, &far, 7);
            assert!(!whole.is_empty(), "{ingress}<-{egress}");
            if ingress != "responses" && ingress != "anthropic" && ingress != "cohere" {
                assert_eq!(pieces, whole, "{ingress}<-{egress}");
            }
            let mut r = Relay::new(relay_ctx(ingress, egress, true));
            for piece in far.chunks(5) {
                let _ = r.feed(HOST, piece);
            }
            let end = r.end(HOST);
            assert!(!end.failed, "{ingress}<-{egress}");
            assert!(end.usage.is_some(), "{ingress}<-{egress}: usage");
            checked += 1;
        }
    }
    assert_eq!(checked, 36);
}

/// A stream cut after its first byte ends on the caller's own in-band error; one cut before any

#[test]
fn a_cut_stream_ends_on_the_callers_error_frame() {
    for ingress in SIX {
        let far = native_stream("openai");
        let mut r = Relay::new(relay_ctx(ingress, "openai", true));
        let _ = r.feed(HOST, &far[..far.len() / 2]);
        let cut = r.cut(HOST, true);
        assert_eq!(cut.reason, "mid-stream");
        assert!(cut.partial);
        assert!(cut.bytes.is_some_and(|b| !b.is_empty()), "{ingress}");

        let mut r = Relay::new(relay_ctx(ingress, "openai", true));
        let cut = r.cut(HOST, true);
        assert_eq!(cut.reason, "pre-first-byte-transport");
        assert!(!cut.partial && cut.bytes.is_none() && cut.usage.is_none());
    }
}

/// A caller that asked for its stream as a JSON array reads one array, under `application/json`.
#[test]
fn a_json_array_caller_reads_its_stream_as_one_array() {
    let far = native_stream("gemini");
    let mut ctx = relay_ctx("gemini", "gemini", true);
    ctx.json_array = true;
    let out = relay_all(&mut Relay::new(ctx), &far, 11);
    let v: Value = serde_json::from_slice(&out).expect("one JSON array");
    assert!(v.as_array().is_some_and(|a| !a.is_empty()), "{v}");
    assert_eq!(
        relay::content_type("gemini", "gemini", true, true),
        ContentType::Json
    );
    assert!(relay::takes_whole("openai", "anthropic", false, false));
    assert!(!relay::takes_whole("openai", "openai", false, false));
    assert!(relay::takes_whole("openai", "openai", false, true));
    assert!(!relay::takes_whole("openai", "anthropic", true, false));
    assert!(!relay::takes_whole("openai", "openai", true, true));
}

use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::stream_intent;
use busbar_plane_llm::exchange::reply::{At, Fault, Reply, ReplyCtx, Verdict};
use busbar_plane_llm::exchange::shaping::Lane;

fn lane(dialect: &'static str) -> Lane {
    Lane {
        model: "m-1".to_string(),
        provider: "p".to_string(),
        dialect,
        path: None,
        path_base: None,
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
    }
}

fn arrival(dialect: &'static str, stream: bool) -> Arrived {
    let body = serde_json::json!({"model": "p", "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]});
    Arrived {
        dialect,
        operation: OpVerb::CHAT,
        model: "p".to_string(),
        content_type: "application/json".to_string(),
        body: serde_json::to_vec(&body).unwrap(),
        parsed: Some(body),
        path: "/v1/chat/completions".to_string(),
        query: None,
        path_model: None,
    }
}

const AT: At = At {
    now_s: 1_752_000_000,
    elapsed_ms: Some(3),
};

/// A relayed stream: the caller's head on the first piece with an `Ok`, the bytes as frames

#[test]
fn a_reply_relays_a_stream_piece_by_piece() {
    let arrived = arrival("openai", true);
    let lane = lane("anthropic");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let far = native_stream("anthropic");
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let pieces: Vec<&[u8]> = far.chunks(40).collect();
    let mut body = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        let piece = reply.feed(HOST, &ctx, p, last, AT);
        if i == 0 {
            let h = piece.head.clone().expect("the head rides the first piece");
            assert_eq!(h.status, 200);
            assert_eq!(
                field(&h.fields, "content-type"),
                Some(b"text/event-stream".as_slice())
            );
            assert_eq!(piece.verdict, Verdict::Ok);
        } else {
            assert!(piece.head.is_none());
        }
        assert_eq!(piece.done, last);
        body.extend_from_slice(&piece.bytes);
        if last {
            assert_eq!(piece.verdict, Verdict::Ok);
            assert_eq!(piece.fault, None);
            assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (11, 7));
        }
    }
    let text = String::from_utf8(body).expect("event text");
    assert!(
        text.starts_with("data: ") && text.ends_with("data: [DONE]\n\n"),
        "{text}"
    );
}

/// A whole answer: nothing for the caller until the last piece, then the head, the translated

#[test]
fn a_reply_takes_a_whole_answer_whole() {
    let arrived = arrival("openai", false);
    let lane = lane("cohere");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let (_, far) = native_answers("cohere")
        .into_iter()
        .next()
        .expect("an answer");
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let (a, b) = far.split_at(far.len() / 2);
    let first = reply.feed(HOST, &ctx, a, false, AT);
    assert!(first.head.is_none() && first.bytes.is_empty() && !first.done);
    assert_eq!(first.verdict, Verdict::None);
    let last = reply.feed(HOST, &ctx, b, true, AT);
    assert_eq!(last.head.map(|h| h.status), Some(200));
    assert!(last.done);
    assert_eq!(last.verdict, Verdict::Ok);
    assert!(last.units.tokens_in > 0 && last.units.tokens_out > 0);
    let v: Value = serde_json::from_slice(&last.bytes).expect("JSON");
    assert_eq!(v["object"], "chat.completion", "{v}");
}

/// A TRANSLATED WHOLE ANSWER BILLS ON AN EARLY LEAVE. The door writes a

#[test]
fn a_whole_answer_handed_to_the_caller_bills_when_the_caller_leaves_early() {
    let arrived = arrival("openai", false);
    let lane = lane("cohere");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let (_, far) = native_answers("cohere")
        .into_iter()
        .next()
        .expect("an answer");
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"application/json")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let last = reply.feed(HOST, &ctx, &far, true, AT);
    assert_eq!(last.verdict, Verdict::Ok);
    assert!(last.units.tokens_in > 0 && last.units.tokens_out > 0);
    assert!(
        reply.partial(),
        "a delivered whole answer must bill its stated units when the caller leaves before its \
         last window"
    );

    let tight = ReplyCtx {
        translate_cap: 4,
        ..ctx
    };
    let mut over = Reply::new(HOST, &tight, 200, head);
    let refused = over.feed(HOST, &tight, &far, true, AT);
    assert_eq!(refused.verdict, Verdict::Hard);
    assert!(
        !over.partial(),
        "an end that delivers nothing bills nothing"
    );
}

/// A far-end error: a client fault is `Hard` with the caller's answer; a transient is `Retry`

#[test]
fn a_reply_judges_a_far_end_error() {
    let arrived = arrival("openai", false);
    let lane = lane("anthropic");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"too long"}}"#;
    for (status, verdict, disposition) in [
        (400, Verdict::Hard, Disposition::ClientFault),
        (529, Verdict::Retry, Disposition::TransientUpstream),
        (401, Verdict::Hard, Disposition::HardDown),
    ] {
        let mut reply = Reply::new(HOST, &ctx, status, JSON_HEAD);
        let (a, b) = body.split_at(10);
        assert!(!reply.feed(HOST, &ctx, a, false, AT).done);
        let piece = reply.feed(HOST, &ctx, b, true, AT);
        assert!(piece.done);
        assert_eq!(piece.verdict, verdict, "{status}");
        assert!(matches!(
            piece.fault,
            Some(Fault::Judged { disposition: d, class: Some(_) }) if d == disposition
        ));
        assert!(piece.head.is_some());
    }
}

/// THE BREAKER'S READING beside the walk's verdict:

#[test]
fn a_judged_far_end_error_carries_the_breakers_reading_apart_from_its_verdict() {
    use busbar_contract::abi::transport::{FAULT_CALLER, FAULT_HARD, FAULT_NONE, FAULT_TRANSIENT};
    use busbar_plane_llm::plane_door::breaker_fault;
    let arrived = arrival("openai", false);
    let mut lane = lane("openai");
    lane.error_map = map(&[("1302", "rate_limit")]);
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let error = |code: &str| {
        format!(r#"{{"error":{{"message":"m","type":"invalid_request_error","code":"{code}"}}}}"#)
    };
    for (status, code, verdict, reading) in [
        (400, "context_length_exceeded", Verdict::Retry, FAULT_NONE),
        (400, "1302", Verdict::Retry, FAULT_TRANSIENT),
        (400, "bad_field", Verdict::Hard, FAULT_CALLER),
        (401, "invalid_api_key", Verdict::Hard, FAULT_HARD),
        (503, "busy", Verdict::Retry, FAULT_TRANSIENT),
    ] {
        let mut reply = Reply::new(HOST, &ctx, status, JSON_HEAD);
        let body = error(code);
        let piece = reply.feed(HOST, &ctx, body.as_bytes(), true, AT);
        assert!(piece.done, "{status} {code}");
        assert_eq!(piece.verdict, verdict, "{status} {code}");
        assert_eq!(
            breaker_fault(piece.fault.as_ref()),
            reading,
            "{status} {code}"
        );
    }

    assert_eq!(
        breaker_fault(Some(&Fault::Transient("upstream-generation-failed"))),
        FAULT_TRANSIENT
    );
    assert_eq!(breaker_fault(None), FAULT_NONE);
}

/// A cut: a stream after its first byte ends on the caller's error frame, `Hard`, the far end's

#[test]
fn a_reply_that_is_cut_says_so() {
    let arrived = arrival("openai", true);
    let lane = lane("anthropic");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let far = native_stream("anthropic");
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let _ = reply.feed(HOST, &ctx, &far[..far.len() / 2], false, AT);
    let cut = reply.cut(HOST, &ctx, true);
    assert!(cut.done && !cut.bytes.is_empty());
    assert_eq!(cut.verdict, Verdict::Hard);
    assert_eq!(cut.fault, Some(Fault::Transient("mid-stream")));

    let arrived = arrival("openai", false);
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let mut reply = Reply::new(HOST, &ctx, 200, JSON_HEAD);
    let _ = reply.feed(HOST, &ctx, b"{\"id\":", false, AT);
    let cut = reply.cut(HOST, &ctx, true);
    assert_eq!(cut.head.map(|h| h.status), Some(502));
    assert_eq!(cut.fault, Some(Fault::Transient("transport")));
}

/// A far end's stream of `far`, relayed to a caller of `caller`: the paths the relay dropped.
fn stream_drops(caller: &str, far: &str, bytes: &[u8]) -> Vec<String> {
    let mut ctx = relay_ctx(caller, far, true);
    ctx.client_include_usage = true;
    let mut r = Relay::new(ctx);
    for piece in bytes.chunks(7) {
        let _ = r.feed(HOST, piece);
    }
    r.end(HOST).dropped
}

/// The far end's spec-shaped stream of `dialect` (the OpenAI stream itself, or as busbar writes it

fn far_stream(dialect: &str) -> Vec<u8> {
    if dialect == "openai" {
        openai_stream()
    } else {
        native_stream(dialect)
    }
}

/// RED ARM of the stream walk: a stream of members every dialect's map or code carries drops

#[test]
fn a_translated_stream_of_carried_members_drops_nothing() {
    for far in SIX {
        let bytes = far_stream(far);
        for caller in SIX.into_iter().filter(|c| *c != far) {
            assert_eq!(
                stream_drops(caller, far, &bytes),
                Vec::<String>::new(),
                "{far} -> {caller}"
            );
        }
    }
}

/// A member the caller's dialect has no form for is dropped on the one drop path, named by its wire

#[test]
fn a_translated_stream_names_each_unmapped_path_once() {
    let far = String::from_utf8(openai_stream())
        .unwrap()
        .replace("\"object\":", "\"obfuscation\":\"x\",\"object\":");
    assert_eq!(
        stream_drops("anthropic", "openai", far.as_bytes()),
        vec!["obfuscation".to_string()]
    );

    let mut far = far_stream("anthropic");
    for _ in 0..2 {
        far.extend_from_slice(b"event: zz_future\ndata: {\"type\":\"zz_future\",\"x\":1}\n\n");
    }
    assert_eq!(
        stream_drops("openai", "anthropic", &far),
        vec!["type=zz_future".to_string()]
    );

    let mut far = Vec::new();
    for _ in 0..2 {
        far.extend_from_slice(
            b"event: response.zz_future\ndata: {\"type\":\"response.zz_future\",\"x\":1}\n\n",
        );
    }
    far.extend_from_slice(&far_stream("responses"));
    assert_eq!(
        stream_drops("openai", "responses", &far),
        vec!["type=response.zz_future".to_string()]
    );

    let mut far =
        busbar_plane_llm::codec::eventstream::encode_frame("zzNeverHeardOf", br#"{"zz":{"b":1}}"#);
    far.extend_from_slice(&far_stream("bedrock"));
    assert_eq!(
        stream_drops("anthropic", "bedrock", &far),
        vec!["zzNeverHeardOf".to_string()]
    );
}

/// Responses refusal events are mapped content: the actual relay carries each text delta into

#[test]
fn a_translated_responses_refusal_keeps_its_text_usage_and_has_no_drop() {
    let events = [
        (
            "response.created",
            serde_json::json!({"type":"response.created", "response":{
            "id":"resp-refusal", "object":"response", "created_at":1, "model":"m-1",
            "status":"in_progress", "output":[]}}),
        ),
        (
            "response.refusal.delta",
            serde_json::json!({"type":"response.refusal.delta",
            "item_id":"m1", "output_index":0, "content_index":0, "delta":"no", "sequence_number":1}),
        ),
        (
            "response.refusal.delta",
            serde_json::json!({"type":"response.refusal.delta",
            "item_id":"m1", "output_index":0, "content_index":0, "delta":" thanks", "sequence_number":2}),
        ),
        (
            "response.refusal.done",
            serde_json::json!({"type":"response.refusal.done",
            "item_id":"m1", "output_index":0, "content_index":0, "refusal":"no thanks", "sequence_number":3}),
        ),
        (
            "response.completed",
            serde_json::json!({"type":"response.completed", "sequence_number":4,
            "response":{"id":"resp-refusal", "object":"response", "created_at":1, "model":"m-1",
                "status":"completed", "output":[{"type":"message", "id":"m1", "role":"assistant",
                    "status":"completed", "content":[{"type":"refusal", "refusal":"no thanks"}]}],
                "usage":{"input_tokens":7, "output_tokens":9, "total_tokens":16}}}),
        ),
    ];
    let mut far = Vec::new();
    for (name, data) in events {
        far.extend_from_slice(format!("event: {name}\ndata: {data}\n\n").as_bytes());
    }
    let mut ctx = relay_ctx("openai", "responses", true);
    ctx.client_include_usage = true;
    let mut relay = Relay::new(ctx);
    let mut bytes = Vec::new();
    for piece in far.chunks(7) {
        if let (Fed::Bytes(part), _) = relay.feed(HOST, piece) {
            bytes.extend_from_slice(&part);
        }
    }
    let end = relay.end(HOST);
    bytes.extend_from_slice(&end.bytes);
    let text = String::from_utf8(bytes).expect("Chat SSE is UTF-8");
    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("each Chat data frame is valid JSON"))
        .collect();
    let refusal: String = chunks
        .iter()
        .filter_map(|chunk| {
            chunk
                .pointer("/choices/0/delta/refusal")
                .and_then(Value::as_str)
        })
        .collect();
    assert_eq!(refusal, "no thanks", "{text}");
    assert!(
        chunks.iter().all(|chunk| chunk
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)),
        "{text}"
    );
    assert!(end.dropped.is_empty(), "{:?}", end.dropped);
    assert!(!end.failed, "{end:?}");
    assert!(end.stream_fault.is_none(), "{end:?}");
    let usage = end.usage.expect("reported usage is retained");
    assert_eq!((usage.input, usage.output), (7, 9));
}

/// A same-dialect stream is a relay: its reader is a tap and drops nothing, whatever it carries.
#[test]
fn a_same_dialect_stream_drops_nothing() {
    let far = String::from_utf8(openai_stream())
        .unwrap()
        .replace("\"object\":", "\"obfuscation\":\"x\",\"object\":");
    assert!(stream_drops("openai", "openai", far.as_bytes()).is_empty());
}

/// The legacy engine's answer tests (`crates/busbar-llm/src/engine/**`), rehomed onto the served

mod rehomed_answers {
    use super::*;
    use std::borrow::Cow;

    use busbar_contract::abi::transport::{FAULT_HARD, FAULT_TRANSIENT};
    use busbar_contract::upstream::CanonicalSignal;
    use busbar_plane_llm::codec::eventstream;
    use busbar_plane_llm::exchange::arrive::PathModel;
    use busbar_plane_llm::exchange::attempt::StreamIntent;
    use busbar_plane_llm::exchange::reply::{failure, wire, Piece};
    use busbar_plane_llm::plane_door::breaker_fault;
    use serde_json::json;

    const SSE_HEAD: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    const EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

    /// The host's entropy, so a dialect that mints an id (a request id, a message id) mints one:

    struct SevensHost;

    impl busbar_contract::codec::CodecHost for SevensHost {
        fn fill_entropy(&self, out: &mut [u8]) -> bool {
            out.fill(7);
            true
        }

        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }

    /// The handle every exchange and codec call in this module is handed (it shadows the file's

    const HOST: &dyn busbar_contract::codec::CodecHost = &SevensHost;

    /// A chat request in `dialect`'s own shape.
    fn chat_request(dialect: &str, stream: bool) -> Value {
        match dialect {
            "anthropic" => json!({"model": "p", "max_tokens": 64, "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]}),
            "responses" => json!({"model": "p", "stream": stream, "input": "hi"}),
            "gemini" => json!({"model": "p", "stream": stream,
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
            "bedrock" => json!({"model": "p", "stream": stream,
                "messages": [{"role": "user", "content": [{"text": "hi"}]}]}),
            _ => json!({"model": "p", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]}),
        }
    }

    /// The caller's arrival of `request` in `dialect`: a path-model dialect's URL names the model

    fn arrived(dialect: &'static str, request: Value, json_array: bool) -> Arrived {
        let stream = request["stream"].as_bool().unwrap_or(false);
        Arrived {
            dialect,
            operation: OpVerb::CHAT,
            model: "p".to_string(),
            content_type: "application/json".to_string(),
            body: serde_json::to_vec(&request).expect("JSON"),
            parsed: Some(request),
            path: String::new(),
            query: None,
            path_model: matches!(dialect, "gemini" | "bedrock").then_some(PathModel {
                stream,
                json_array,
                model_not_found_message: None,
            }),
        }
    }

    /// A `caller` asking a far end of `far` for a chat answer, streamed when `stream`.
    fn ask(caller: &'static str, far: &'static str, stream: bool) -> (Arrived, Lane) {
        (
            arrived(caller, chat_request(caller, stream), false),
            lane(far),
        )
    }

    fn reply_ctx<'a>(arrived: &'a Arrived, lane: &'a Lane, passthrough: bool) -> ReplyCtx<'a> {
        let wants_stream = arrived
            .parsed
            .as_ref()
            .and_then(|request| request["stream"].as_bool())
            .unwrap_or(false);
        ReplyCtx {
            arrived,
            lane,
            intent: StreamIntent {
                wants_stream,
                client_include_usage: false,
            },
            passthrough,
            translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
        }
    }

    /// A piece the caller read, its bytes its own.
    fn owned(piece: Piece<'_>) -> Piece<'static> {
        Piece {
            head: piece.head,
            bytes: Cow::Owned(piece.bytes.into_owned()),
            units: piece.units,
            verdict: piece.verdict,
            fault: piece.fault,
            tap_fault: piece.tap_fault,
            done: piece.done,
            dropped: piece.dropped,
            synthesized: piece.synthesized,
        }
    }

    /// What the caller read of one answer.
    struct Read {
        status: u16,
        fields: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
        pieces: Vec<Piece<'static>>,
    }

    impl Read {
        /// The piece that ended the answer.
        fn end(&self) -> &Piece<'static> {
            self.pieces
                .iter()
                .find(|piece| piece.done)
                .or(self.pieces.last())
                .expect("a piece")
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|err| panic!("a JSON body ({err}): {}", self.text()))
        }

        /// How many head fields are named `name`.
        fn count(&self, name: &str) -> usize {
            self.fields
                .iter()
                .filter(|(field_name, _)| field_name == name)
                .count()
        }

        /// The first head field named `name`, as text.
        fn value(&self, name: &str) -> Option<String> {
            field(&self.fields, name).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        }
    }

    /// One answer through the served reply: the far end's `status` and `head`, its body in

    fn answer(
        ctx: &ReplyCtx<'_>,
        status: u16,
        head: &[(&[u8], &[u8])],
        pieces: &[&[u8]],
        cut: bool,
    ) -> Read {
        let mut reply = Reply::new(HOST, ctx, status, head);
        let mut out: Vec<Piece<'static>> = Vec::new();
        for (index, bytes) in pieces.iter().enumerate() {
            let last = index + 1 == pieces.len() && !cut;
            out.push(owned(reply.feed(HOST, ctx, bytes, last, AT)));
        }
        if pieces.is_empty() {
            out.push(owned(reply.feed(HOST, ctx, &[], !cut, AT)));
        }
        if cut {
            out.push(reply.cut(HOST, ctx, true));
        }
        let mut read = Read {
            status: 0,
            fields: Vec::new(),
            body: Vec::new(),
            pieces: Vec::new(),
        };
        for piece in &out {
            if let (Some(head), 0) = (&piece.head, read.status) {
                read.status = head.status;
                read.fields = head.fields.clone();
            }
            read.body.extend_from_slice(&piece.bytes);
        }
        read.pieces = out;
        read
    }

    /// The `(event, data)` frames of an event stream, the `[DONE]` sentinel left out.
    fn sse_frames(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for frame in text.split("\n\n") {
            let mut event = String::new();
            let mut data: Option<String> = None;
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data = Some(rest.trim().to_string());
                }
            }
            if let Some(data) = data.filter(|payload| payload != "[DONE]") {
                out.push((event, data));
            }
        }
        out
    }

    /// The JSON payload of every `data:` frame of an event stream.
    fn sse_payloads(text: &str) -> Vec<Value> {
        sse_frames(text)
            .into_iter()
            .filter_map(|(_, data)| serde_json::from_str(&data).ok())
            .collect()
    }

    /// `events` framed as a far end's `data:` stream, ended by the `[DONE]` frame.
    fn data_stream(events: &[&str]) -> Vec<u8> {
        let mut out: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        out += "data: [DONE]\n\n";
        out.into_bytes()
    }

    /// The legacy engine's chat stream fixture: a role delta, the text, the stop with its usage.
    fn chat_stream_events() -> Vec<u8> {
        data_stream(&[
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#,
        ])
    }

    /// A chat completion of `text`, as a far end of the chat dialect answers it whole.
    fn chat_completion(text: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "chatcmpl-ok", "object": "chat.completion", "created": 1_234_567_890,
            "model": "glm-4.5",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        }))
        .expect("JSON")
    }

    /// A binary event-stream body decoded: its frames' event names and whatever bytes were left.
    fn eventstream_names(body: &[u8]) -> (Vec<String>, usize) {
        let mut buf = body.to_vec();
        let frames = eventstream::drain_frames_checked(&mut buf, None).0;
        (
            frames.into_iter().map(|(name, _)| name).collect(),
            buf.len(),
        )
    }

    /// A UUID-v4-shaped id: 8-4-4-4-12 lowercase hex.
    fn is_uuid(id: &str) -> bool {
        id.split('-').map(str::len).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
            && id
                .chars()
                .all(|ch| ch == '-' || (ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()))
    }

    /// `dialect`'s own error envelope for `status`, `kind` and `message`, written by its codec.
    fn own_envelope(dialect: &str, status: u16, kind: &str, message: &str) -> Value {
        busbar_plane_llm::codec::DECLS
            .iter()
            .find(|decl| decl.name == dialect)
            .and_then(|decl| decl.dialect())
            .expect("the dialect has a codec")
            .write_error(HOST, status, kind, message)
    }

    /// An envelope with the request id its writer mints taken out.
    fn without_minted_id(mut envelope: Value) -> Value {
        if let Some(obj) = envelope.as_object_mut() {
            obj.remove("request_id");
        }
        envelope
    }

    const SPLIT_HEAD: &[u8] = br#"{"id":"c-1","finish_re"#;

    /// The second half of a split same-dialect body whose stop reason is `finish`.
    fn split_tail(finish: &str) -> Vec<u8> {
        format!(
            r#"ason":"{finish}","message":{{"role":"assistant","content":[{{"type":"text","text":"x"}}]}},"usage":{{"tokens":{{"input_tokens":10,"output_tokens":5}}}}}}"#
        )
        .into_bytes()
    }

    /// A same-dialect whole body in two pieces through a relay that meters nothing (the ungoverned

    fn ungoverned_relay(first: &[u8], second: &[u8]) -> (Vec<u8>, relay::End) {
        let mut ctx = relay_ctx("cohere", "cohere", false);
        ctx.meter = false;
        let mut relay = Relay::new(ctx);
        let mut out = Vec::new();
        for piece in [first, second] {
            if let (Fed::Bytes(bytes), _) = relay.feed(HOST, piece) {
                out.extend_from_slice(&bytes);
            }
        }
        let end = relay.end(HOST);
        out.extend_from_slice(&end.bytes);
        (out, end)
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::ungoverned_same_protocol_cohere_error_split_key_faults_the_breaker`

    #[test]
    fn ungoverned_same_protocol_cohere_error_split_key_faults_the_breaker() {
        let tail = split_tail("ERROR");
        let (out, end) = ungoverned_relay(SPLIT_HEAD, &tail);
        assert_eq!(
            out,
            [SPLIT_HEAD, tail.as_slice()].concat(),
            "bytes unchanged"
        );
        assert!(end.generation_failed && end.failed, "the end is an error");
        assert_eq!(end.usage, None, "an ungoverned relay reads no usage");

        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let read = answer(&ctx, 200, JSON_HEAD, &[SPLIT_HEAD, &tail], false);
        assert_eq!(read.body, [SPLIT_HEAD, tail.as_slice()].concat());
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(
            read.end().fault,
            Some(Fault::Transient("upstream-generation-failed"))
        );
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::ungoverned_same_protocol_cohere_complete_split_key_is_no_fault`

    #[test]
    fn ungoverned_same_protocol_cohere_complete_split_key_is_no_fault() {
        let tail = split_tail("COMPLETE");
        let (out, end) = ungoverned_relay(SPLIT_HEAD, &tail);
        assert_eq!(out, [SPLIT_HEAD, tail.as_slice()].concat());
        assert!(!end.generation_failed && !end.failed, "a clean end");

        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let read = answer(&ctx, 200, JSON_HEAD, &[SPLIT_HEAD, &tail], false);
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.end().fault, None);
    }

    const FAR_ANSWER_HEAD: &[(&[u8], &[u8])] = &[
        (b"content-type", b"application/json"),
        (b"x-ratelimit-remaining-tokens", b"99"),
        (b"openai-processing-ms", b"12"),
        (b"openai-organization", b"org-operator"),
        (b"openai-project", b"proj-operator"),
    ];

    /// DIALECT-FIDELITY F2: a same-dialect 2xx answer carries the far head

    #[test]
    fn a_same_dialect_answer_relays_the_upstream_head() {
        let (arrived, lane) = ask("openai", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("hi");
        let read = answer(&ctx, 200, FAR_ANSWER_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(
            read.count("x-ratelimit-remaining-tokens"),
            0,
            "{:?}",
            read.fields
        );
        assert_eq!(read.value("openai-processing-ms").as_deref(), Some("12"));
        assert_eq!(read.count("openai-organization"), 0, "{:?}", read.fields);
        assert_eq!(read.count("openai-project"), 0, "{:?}", read.fields);
    }

    /// A same-dialect 400 `invalid_request_error` reaches the caller with the far head's values

    #[test]
    fn a_same_dialect_error_relays_the_upstream_head() {
        let (arrived, lane) = ask("openai", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let mut head = FAR_ANSWER_HEAD.to_vec();
        head.push((b"retry-after", b"7"));
        let body = br#"{"error":{"message":"bad","type":"invalid_request_error"}}"#;
        let read = answer(&ctx, 400, &head, &[body], false);
        assert_eq!(read.status, 400);
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(
            read.count("x-ratelimit-remaining-tokens"),
            0,
            "{:?}",
            read.fields
        );
        assert_eq!(read.value("retry-after").as_deref(), Some("7"));
        assert_eq!(read.count("openai-organization"), 0, "{:?}", read.fields);
        assert_eq!(read.body, body, "the far end's bytes");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/client_header_forwarding_tests.rs::a_translated_answer_relays_no_upstream_head`

    #[test]
    fn a_translated_answer_relays_no_upstream_head() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("hi");
        let read = answer(&ctx, 200, FAR_ANSWER_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        for name in [
            "x-ratelimit-remaining-tokens",
            "openai-processing-ms",
            "openai-organization",
            "openai-project",
        ] {
            assert_eq!(read.count(name), 0, "{name}: {:?}", read.fields);
        }
    }

    /// OWNER 2026-10-27 (busbar answers in the caller's dialect AND mode): a caller that asked for a

    #[test]
    fn a_stream_synthesised_from_a_whole_answer_is_marked_and_billed_from_the_body() {
        let body = chat_completion("hi");
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(
            read.value("content-type").as_deref(),
            Some("text/event-stream"),
            "{:?}",
            read.fields
        );
        let end = read.end();
        assert!(end.synthesized, "the synthesised stream is marked");
        assert_eq!(
            (end.units.tokens_in, end.units.tokens_out),
            (7, 3),
            "billed from the far end's whole body"
        );

        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert!(!read.end().synthesized);
        assert_eq!(
            (read.end().units.tokens_in, read.end().units.tokens_out),
            (7, 3)
        );
    }

    /// OWNER 2026-10-27 (busbar answers in the caller's dialect AND mode) holds within one dialect

    #[test]
    fn a_same_dialect_whole_answer_to_a_stream_ask_is_the_callers_stream() {
        let response = serde_json::to_vec(&json!({
            "id": "resp_ok", "object": "response", "status": "completed", "model": "m-1",
            "created_at": 0, "error": null, "incomplete_details": null, "instructions": null,
            "metadata": null, "parallel_tool_calls": true, "temperature": null, "top_p": null,
            "tool_choice": "auto", "tools": [],
            "output": [{"type": "message", "id": "msg_ok", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "hi", "annotations": []}]}],
            "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
        }))
        .expect("JSON");
        for (dialect, body, last) in [
            ("openai", chat_completion("hi"), "data: [DONE]"),
            ("responses", response, "response.completed"),
        ] {
            let (arrived, lane) = ask(dialect, dialect, true);
            let ctx = reply_ctx(&arrived, &lane, false);
            let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
            assert_eq!(read.status, 200, "{dialect}");
            assert_eq!(
                read.value("content-type").as_deref(),
                Some("text/event-stream"),
                "{dialect}: {:?} {}",
                read.fields,
                read.text()
            );
            assert!(read.text().contains(last), "{dialect}: {}", read.text());
            let end = read.end();
            assert!(
                end.synthesized,
                "{dialect}: the synthesised stream is marked"
            );
            assert_eq!(
                (end.units.tokens_in, end.units.tokens_out),
                (7, 3),
                "{dialect}: billed from the far end's whole body"
            );
        }
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_pool_integration_tests.rs::test_sse_events_single_data_prefix`

    #[test]
    fn test_sse_events_single_data_prefix() {
        let (arrived, lane) = ask("anthropic", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = data_stream(&["event-0", "event-1"]);
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        let text = read.text();
        assert!(!text.contains("data: data:"), "{text}");
        assert!(
            text.contains("data: event-0\n\n") && text.contains("data: event-1\n\n"),
            "{text}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_pool_integration_tests.rs::test_stream_inspection_tap_usage_parsing`

    #[test]
    fn test_stream_inspection_tap_usage_parsing() {
        let (arrived, lane) = ask("anthropic", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = data_stream(&[
            r#"{"type":"message_start"}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"message_delta","usage":{"input_tokens":10,"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let pieces: Vec<&[u8]> = far.chunks(17).collect();
        let read = answer(&ctx, 200, SSE_HEAD, &pieces, false);
        assert_eq!(read.status, 200);
        assert_eq!(read.text(), String::from_utf8_lossy(&far), "byte-identical");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_pool_integration_tests.rs::test_forward_once_cross_protocol_auth_kinds_match_main_path`

    #[test]
    fn test_forward_once_cross_protocol_auth_kinds_match_main_path() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = br#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        for status in [401, 403] {
            let read = answer(&ctx, status, JSON_HEAD, &[body], false);
            assert_eq!(read.status, 401, "{status}");
            let envelope = read.json();
            assert_eq!(envelope["type"], "error", "{status}: {envelope}");
            assert_eq!(
                envelope["error"]["type"], "authentication_error",
                "{status}: {envelope}"
            );
            assert!(
                !read.text().contains("Incorrect API key"),
                "{status}: {envelope}"
            );
            assert_eq!(read.end().verdict, Verdict::Hard, "{status}");
            assert_eq!(
                breaker_fault(read.end().fault.as_ref()),
                FAULT_HARD,
                "{status}"
            );
        }
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_pool_integration_tests.rs::test_cross_protocol_stream_openai_lane_to_anthropic_client`

    #[test]
    fn test_cross_protocol_stream_openai_lane_to_anthropic_client() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = data_stream(&[
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ]);
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        let text = read.text();
        for needle in [
            "event: message_start",
            "event: content_block_delta",
            "text_delta",
            "\"hi\"",
            "event: message_stop",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        assert!(!text.contains("chat.completion.chunk"), "{text}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_error_kind_mapping`

    #[test]
    fn test_cross_protocol_error_kind_mapping() {
        for (status, kind) in [
            (401, "authentication_error"),
            (403, "permission_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (502, "api_error"),
            (503, "overloaded"),
            (504, "timeout"),
            (400, "invalid_request_error"),
            (404, "invalid_request_error"),
        ] {
            assert_eq!(wire::cross_protocol_error_kind(status), kind, "{status}");
        }
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_shape_cross_protocol_error_auth_kinds`

    #[test]
    fn test_shape_cross_protocol_error_auth_kinds() {
        let body = br#"{"error":{"message":"nope"}}"#;
        for (status, kind) in [(401, "authentication_error"), (403, "permission_error")] {
            let far = FarError {
                status,
                head: JSON_HEAD,
                body,
            };
            let relayed = failure::relay(HOST, "anthropic", "openai", &far);
            let judged = judge(
                HOST,
                "anthropic",
                "openai",
                OpVerb::CHAT,
                &HashMap::new(),
                true,
                &far,
            );
            for rendered in [relayed, judged.answer] {
                assert_eq!(rendered.status, status);
                let envelope: Value = serde_json::from_slice(&rendered.body).expect("JSON");
                assert_eq!(envelope["error"]["type"], kind, "{status}: {envelope}");
                assert_eq!(envelope["error"]["message"], "nope", "{status}: {envelope}");
            }
        }
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_ingress_stream_content_type_by_protocol`

    #[test]
    fn test_ingress_stream_content_type_by_protocol() {
        for dialect in ["openai", "anthropic", "gemini", "cohere", "responses"] {
            assert_eq!(
                wire::ingress_stream_content_type(dialect),
                Some("text/event-stream"),
                "{dialect}"
            );
        }
        assert_eq!(
            wire::ingress_stream_content_type("bedrock"),
            Some(EVENTSTREAM)
        );
        assert_eq!(wire::ingress_stream_content_type("nonsense"), None);
        assert_eq!(
            relay::content_type("bedrock", "openai", true, false),
            ContentType::Ingress(EVENTSTREAM)
        );
        assert_eq!(
            relay::content_type("anthropic", "bedrock", true, false),
            ContentType::Ingress("text/event-stream")
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_response_carries_ingress_ct_and_native_id`

    #[test]
    fn test_cross_protocol_response_carries_ingress_ct_and_native_id() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = serde_json::to_vec(&json!({
            "id": "chatcmpl-LEAK123", "object": "chat.completion", "created": 1_234_567_890,
            "system_fingerprint": "fp_backend", "model": "glm-4.5",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        }))
        .expect("JSON");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(
            read.value("content-type").as_deref(),
            Some("application/json")
        );
        let message = read.json();
        assert_eq!(message["type"], "message", "{message}");
        let id = message["id"].as_str().unwrap_or_default();
        assert!(id.starts_with("msg_") && !id.contains("chatcmpl-"), "{id}");
        let text = read.text();
        assert!(
            !text.contains("chatcmpl-LEAK123") && !text.contains("fp_backend"),
            "{text}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_bedrock_ingress_success_carries_amzn_request_id`

    #[test]
    fn test_bedrock_ingress_success_carries_amzn_request_id() {
        let (arrived, lane) = ask("bedrock", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("Hi");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.count("x-amzn-requestid"), 1, "{:?}", read.fields);
        let id = read.value("x-amzn-requestid").unwrap_or_default();
        assert_eq!(id.len(), 36, "{id}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_anthropic_ingress_success_carries_request_id_header`

    #[test]
    fn test_anthropic_ingress_success_carries_request_id_header() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("Hi");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.count("request-id"), 1, "{:?}", read.fields);
        let id = read.value("request-id").unwrap_or_default();
        assert!(id.starts_with("req_"), "{id}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_anthropic_ingress_stream_carries_request_id_header`

    #[test]
    fn test_anthropic_ingress_stream_carries_request_id_header() {
        let (arrived, lane) = ask("anthropic", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], false);
        assert_eq!(read.status, 200);
        let id = read.value("request-id").unwrap_or_default();
        assert!(id.starts_with("req_"), "{id}: {:?}", read.fields);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_client_fault_reshapes_error_envelope`

    #[test]
    fn test_cross_protocol_client_fault_reshapes_error_envelope() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = br#"{"error":{"message":"Invalid 'max_tokens': must be positive","type":"invalid_request_error","param":"max_tokens","code":"invalid_value"}}"#;
        let read = answer(&ctx, 400, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 400);
        assert_eq!(
            read.value("content-type").as_deref(),
            Some("application/json")
        );
        let envelope = read.json();
        assert_eq!(envelope["type"], "error", "{envelope}");
        assert_eq!(
            envelope["error"]["type"], "invalid_request_error",
            "{envelope}"
        );
        let text = read.text();
        assert!(
            !text.contains("\"param\"") && !text.contains("\"code\""),
            "{text}"
        );
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("max_tokens"),
            "{envelope}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_anthropic_same_proto_error_relays_upstream_request_id_verbatim_once`

    #[test]
    fn test_anthropic_same_proto_error_relays_upstream_request_id_verbatim_once() {
        let (arrived, lane) = ask("anthropic", "anthropic", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let head: &[(&[u8], &[u8])] = &[
            (b"content-type", b"application/json"),
            (b"request-id", b"upstream-anthropic-rid-0001"),
        ];
        let body =
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#;
        let read = answer(&ctx, 400, head, &[body], false);
        assert_eq!(read.status, 400);
        assert_eq!(read.count("request-id"), 1, "{:?}", read.fields);
        assert_eq!(
            read.value("request-id").as_deref(),
            Some("upstream-anthropic-rid-0001")
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_anthropic_same_proto_passthrough_401_relays_request_id_verbatim_once`

    #[test]
    fn test_anthropic_same_proto_passthrough_401_relays_request_id_verbatim_once() {
        let (arrived, lane) = ask("anthropic", "anthropic", false);
        let ctx = reply_ctx(&arrived, &lane, true);
        let head: &[(&[u8], &[u8])] = &[
            (b"content-type", b"application/json"),
            (b"request-id", b"upstream-rid-passthrough"),
        ];
        let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let read = answer(&ctx, 401, head, &[body], false);
        assert_eq!(read.status, 401);
        assert_eq!(read.body, body, "the far end's bytes");
        assert_eq!(read.count("request-id"), 1, "{:?}", read.fields);
        assert_eq!(
            read.value("request-id").as_deref(),
            Some("upstream-rid-passthrough")
        );
        assert_eq!(
            read.end().fault,
            Some(Fault::Judged {
                disposition: Disposition::ClientFault,
                class: None
            })
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_bedrock_converse_stream_buffered_cross_protocol_emits_binary_eventstream`

    #[test]
    fn test_bedrock_converse_stream_buffered_cross_protocol_emits_binary_eventstream() {
        let (arrived, lane) = ask("bedrock", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("Hi");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.value("content-type").as_deref(), Some(EVENTSTREAM));
        assert_eq!(
            read.value("x-amzn-requestid").unwrap_or_default().len(),
            36,
            "{:?}",
            read.fields
        );
        let (names, _) = eventstream_names(&read.body);
        assert_eq!(
            names.first().map(String::as_str),
            Some("messageStart"),
            "{names:?}"
        );
        for name in ["contentBlockDelta", "messageStop", "metadata"] {
            assert!(names.iter().any(|seen| seen == name), "{name}: {names:?}");
        }
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_gemini_json_array_buffered_cross_protocol_emits_one_element_array`

    #[test]
    fn test_gemini_json_array_buffered_cross_protocol_emits_one_element_array() {
        let body = chat_completion("Hi");
        let answered = whole::translate(HOST, &ctx("gemini", "openai", true, true), 200, &body);
        assert_eq!(answered.end, WholeEnd::Delivered);
        assert_eq!(answered.answer.status, 200);
        assert_eq!(
            field(&answered.answer.fields, "content-type"),
            Some(b"application/json".as_slice())
        );
        let array: Value = serde_json::from_slice(&answered.answer.body).expect("JSON");
        let elements = array.as_array().expect("an array");
        assert_eq!(elements.len(), 1, "{array}");
        assert!(elements[0].get("candidates").is_some(), "{array}");
        assert!(elements[0].get("choices").is_none(), "{array}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_gemini_json_array_buffered_via_forward_once_matches_primary`

    #[test]
    fn test_gemini_json_array_buffered_via_forward_once_matches_primary() {
        let arrived = arrived("gemini", chat_request("gemini", true), true);
        let lane = lane("openai");
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("Hi");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(
            read.value("content-type").as_deref(),
            Some("application/json")
        );
        let array = read.json();
        let elements = array.as_array().expect("an array");
        assert_eq!(elements.len(), 1, "{array}");
        assert!(elements[0].get("candidates").is_some(), "{array}");
        assert!(elements[0].get("choices").is_none(), "{array}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_non_gemini_stream_buffered_cross_protocol_stays_a_plain_object_not_an_array`

    #[test]
    fn test_non_gemini_stream_buffered_cross_protocol_stays_a_plain_object_not_an_array() {
        let (arrived, lane) = ask("openai", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = br#"{"id":"msg_buf","type":"message","role":"assistant","content":[{"type":"text","text":"Hi"}],"model":"claude-3","stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#;
        let read = answer(&ctx, 200, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 200);
        let content_type = read.value("content-type").unwrap_or_default();
        assert!(
            content_type.starts_with("text/event-stream"),
            "{content_type}"
        );
        let text = read.text();
        assert!(!text.trim_start().starts_with('['), "{text}");
        let payloads = sse_payloads(&text);
        assert!(!payloads.is_empty(), "{text}");
        assert!(payloads.iter().all(Value::is_object), "{text}");
        assert!(
            payloads.iter().any(|chunk| chunk.get("choices").is_some()),
            "{text}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_converse_routes_and_returns_json`

    #[test]
    fn test_bedrock_converse_routes_and_returns_json() {
        let (arrived, lane) = ask("bedrock", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let body = chat_completion("hello there");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("application/json"));
        let id = read.value("x-amzn-requestid").unwrap_or_default();
        assert!(is_uuid(&id), "{id}");
        let converse = read.json();
        assert!(
            converse.get("output").is_some() || converse.get("usage").is_some(),
            "{converse}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_converse_stream_returns_binary_eventstream`

    #[test]
    fn test_bedrock_converse_stream_returns_binary_eventstream() {
        let (arrived, lane) = ask("bedrock", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = chat_stream_events();
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with(EVENTSTREAM));
        let id = read.value("x-amzn-requestid").unwrap_or_default();
        assert!(is_uuid(&id), "{id}");
        let (names, left) = eventstream_names(&read.body);
        assert_eq!(left, 0, "a whole sequence of frames");
        for name in ["messageStart", "contentBlockDelta", "messageStop"] {
            assert!(names.iter().any(|seen| seen == name), "{name}: {names:?}");
        }
    }

    /// The far end's native Converse stream frames.
    fn converse_frames(frames: &[(&str, &[u8])]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|(name, payload)| eventstream::encode_frame(name, payload))
            .collect()
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_same_protocol_stream_passthrough_forwards_upstream_request_id`

    #[test]
    fn test_bedrock_same_protocol_stream_passthrough_forwards_upstream_request_id() {
        let (arrived, lane) = ask("bedrock", "bedrock", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let head: &[(&[u8], &[u8])] = &[
            (b"content-type", EVENTSTREAM.as_bytes()),
            (b"x-amzn-requestid", b"fixed-upstream-amzn-req-id-0001"),
        ];
        let far = converse_frames(&[
            ("messageStart", br#"{"role":"assistant"}"#),
            (
                "contentBlockDelta",
                br#"{"delta":{"text":"hi"},"contentBlockIndex":0}"#,
            ),
            ("messageStop", br#"{"stopReason":"end_turn"}"#),
            (
                "metadata",
                br#"{"usage":{"inputTokens":5,"outputTokens":3,"totalTokens":8}}"#,
            ),
        ]);
        let pieces: Vec<&[u8]> = far.chunks(23).collect();
        let read = answer(&ctx, 200, head, &pieces, false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with(EVENTSTREAM));
        assert_eq!(read.count("x-amzn-requestid"), 1, "{:?}", read.fields);
        assert_eq!(
            read.value("x-amzn-requestid").as_deref(),
            Some("fixed-upstream-amzn-req-id-0001")
        );
        let (names, left) = eventstream_names(&read.body);
        assert_eq!(left, 0);
        assert_eq!(
            names,
            [
                "messageStart",
                "contentBlockDelta",
                "messageStop",
                "metadata"
            ]
        );
        assert_eq!(read.body, far, "the far end's frames, verbatim");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_same_protocol_converse_non_stream_forwards_upstream_request_id`

    #[test]
    fn test_bedrock_same_protocol_converse_non_stream_forwards_upstream_request_id() {
        let (arrived, lane) = ask("bedrock", "bedrock", false);
        let ctx = reply_ctx(&arrived, &lane, false);
        let head: &[(&[u8], &[u8])] = &[
            (b"content-type", b"application/json"),
            (
                b"x-amzn-requestid",
                b"fixed-upstream-amzn-req-id-nonstream-0001",
            ),
        ];
        let body = br#"{"output":{"message":{"role":"assistant","content":[{"text":"hi there"}]}},"stopReason":"end_turn","usage":{"inputTokens":5,"outputTokens":3,"totalTokens":8}}"#;
        let read = answer(&ctx, 200, head, &[body], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("application/json"));
        assert_eq!(read.count("x-amzn-requestid"), 1, "{:?}", read.fields);
        assert_eq!(
            read.value("x-amzn-requestid").as_deref(),
            Some("fixed-upstream-amzn-req-id-nonstream-0001")
        );
        assert_eq!(read.body, body, "the far end's bytes");
    }

    /// The checks every bedrock caller's cut stream answers to: pure binary frames, nothing left

    fn assert_binary_exception_tail(read: &Read) {
        let body = &read.body;
        assert!(
            !body.windows(7).any(|bytes| bytes == b"event: ")
                && !body.windows(6).any(|bytes| bytes == b"data: "),
            "no SSE text in a binary body: {body:?}"
        );
        let (names, left) = eventstream_names(body);
        assert_eq!(left, 0, "a whole sequence of frames");
        assert!(!names.is_empty());
        let raw = String::from_utf8_lossy(body);
        assert!(raw.contains(":exception-type"), "{body:?}");
        assert!(raw.contains("internalServerException"), "{body:?}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_same_protocol_stream_mid_stream_transport_error_appends_binary_exception`

    #[test]
    fn test_bedrock_same_protocol_stream_mid_stream_transport_error_appends_binary_exception() {
        let (arrived, lane) = ask("bedrock", "bedrock", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let head: &[(&[u8], &[u8])] = &[
            (b"content-type", EVENTSTREAM.as_bytes()),
            (b"x-amzn-requestid", b"fixed-upstream-amzn-req-id-err1"),
        ];
        let far = converse_frames(&[("messageStart", br#"{"role":"assistant"}"#)]);
        let read = answer(&ctx, 200, head, &[&far], true);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with(EVENTSTREAM));
        assert_binary_exception_tail(&read);
        assert_eq!(read.end().fault, Some(Fault::Transient("mid-stream")));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_bedrock_ingress_mid_stream_transport_error_appends_binary_exception`

    #[test]
    fn test_bedrock_ingress_mid_stream_transport_error_appends_binary_exception() {
        let (arrived, lane) = ask("bedrock", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        assert_binary_exception_tail(&read);
    }

    /// The last `data:` frame of a stream, as JSON.
    fn last_payload(text: &str) -> Value {
        sse_payloads(text)
            .pop()
            .unwrap_or_else(|| panic!("a trailing data: frame: {text}"))
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_openai_ingress_mid_stream_transport_error_appends_native_sse`

    #[test]
    fn test_openai_ingress_mid_stream_transport_error_appends_native_sse() {
        let (arrived, lane) = ask("openai", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        let text = read.text();
        assert!(!text.contains("event:"), "{text}");
        assert!(last_payload(&text).get("error").is_some(), "{text}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_gemini_stream_generate_content_alt_sse_is_event_stream`

    #[test]
    fn test_gemini_stream_generate_content_alt_sse_is_event_stream() {
        let (arrived, lane) = ask("gemini", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = chat_stream_events();
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"));
        let payloads = sse_payloads(&read.text());
        assert!(!payloads.is_empty());
        assert!(payloads
            .iter()
            .any(|frame| frame.get("candidates").is_some()));
        assert!(payloads.iter().all(|frame| frame.get("choices").is_none()));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_gemini_alt_sse_mid_stream_transport_error_appends_native_sse_frame`

    #[test]
    fn test_gemini_alt_sse_mid_stream_transport_error_appends_native_sse_frame() {
        let (arrived, lane) = ask("gemini", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"));
        let text = read.text();
        assert!(!text.contains("event:"), "{text}");
        assert!(!text.contains("chat.completion.chunk"), "{text}");
        let payloads = sse_payloads(&text);
        assert!(payloads.iter().all(|frame| frame.get("choices").is_none()));
        let last = last_payload(&text);
        assert!(last["error"].get("status").is_some(), "{text}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_gemini_json_array_mid_stream_error_closes_array_no_sse`

    #[test]
    fn test_gemini_json_array_mid_stream_error_closes_array_no_sse() {
        let arrived = arrived("gemini", chat_request("gemini", true), true);
        let lane = lane("openai");
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("application/json"));
        let text = read.text();
        assert!(
            !text.contains("event:") && !text.contains("data:"),
            "{text}"
        );
        let array = read.json();
        let elements = array.as_array().expect("an array");
        assert!(
            elements
                .iter()
                .any(|element| element["error"].get("status").is_some()),
            "{text}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_anthropic_cross_protocol_message_start_full_skeleton`

    #[test]
    fn test_anthropic_cross_protocol_message_start_full_skeleton() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = chat_stream_events();
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        let text = read.text();
        let (_, data) = sse_frames(&text)
            .into_iter()
            .find(|(event, _)| event == "message_start")
            .unwrap_or_else(|| panic!("a message_start: {text}"));
        let event: Value = serde_json::from_str(&data).expect("JSON");
        let message = &event["message"];
        assert!(
            message["id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("msg_"),
            "{message}"
        );
        assert_eq!(message["type"], "message", "{message}");
        assert!(message["content"].is_array(), "{message}");
        assert!(
            message.get("stop_reason").is_some_and(Value::is_null),
            "{message}"
        );
        assert!(
            message.get("stop_sequence").is_some_and(Value::is_null),
            "{message}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_passthrough_401_cross_protocol_reshaped_to_ingress`

    #[test]
    fn test_passthrough_401_cross_protocol_reshaped_to_ingress() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane, true);
        let body = br#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        let read = answer(&ctx, 401, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 401);
        let envelope = read.json();
        assert_eq!(envelope["type"], "error", "{envelope}");
        assert_eq!(
            envelope["error"]["type"], "authentication_error",
            "{envelope}"
        );
        assert_eq!(
            read.end().fault,
            Some(Fault::Judged {
                disposition: Disposition::ClientFault,
                class: None
            })
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_openai_ingress_stream_emits_native_openai_frames`

    #[test]
    fn test_openai_ingress_stream_emits_native_openai_frames() {
        let (arrived, lane) = ask("openai", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = data_stream(&[
            r#"{"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#,
        ]);
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"));
        let text = read.text();
        assert!(!text.contains("event:"), "{text}");
        assert!(text.contains("[DONE]"), "{text}");
        let objects: Vec<Value> = sse_payloads(&text)
            .into_iter()
            .filter_map(|chunk| chunk.get("object").cloned())
            .collect();
        assert!(!objects.is_empty(), "{text}");
        assert!(
            objects.iter().all(|tag| tag == "chat.completion.chunk"),
            "{objects:?}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_cohere_ingress_stream_emits_native_cohere_frames`

    #[test]
    fn test_cohere_ingress_stream_emits_native_cohere_frames() {
        let (arrived, lane) = ask("cohere", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = chat_stream_events();
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"));
        let text = read.text();
        assert!(!text.contains("chat.completion.chunk"), "{text}");
        let types: Vec<String> = sse_payloads(&text)
            .into_iter()
            .filter_map(|frame| frame["type"].as_str().map(str::to_string))
            .collect();
        assert!(
            types.iter().any(|kind| kind == "content-delta"),
            "{types:?}"
        );
        let start = types.iter().position(|kind| kind == "message-start");
        let end = types.iter().rposition(|kind| kind == "message-end");
        assert!(
            matches!((start, end), (Some(first), Some(last)) if first < last),
            "{types:?}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_responses_ingress_stream_emits_native_responses_events`

    #[test]
    fn test_responses_ingress_stream_emits_native_responses_events() {
        let (arrived, lane) = ask("responses", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = chat_stream_events();
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read
            .value("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"));
        let text = read.text();
        assert!(!text.contains("chat.completion.chunk"), "{text}");
        let events: Vec<String> = sse_frames(&text)
            .into_iter()
            .map(|(event, _)| event)
            .collect();
        assert!(
            events
                .iter()
                .any(|event| event == "response.output_text.delta"),
            "{events:?}"
        );
        let created = events.iter().position(|event| event == "response.created");
        let completed = events
            .iter()
            .rposition(|event| event == "response.completed");
        assert!(
            matches!((created, completed), (Some(first), Some(last)) if first < last),
            "{events:?}"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_cohere_ingress_mid_stream_transport_error_appends_native_sse`

    #[test]
    fn test_cohere_ingress_mid_stream_transport_error_appends_native_sse() {
        let (arrived, lane) = ask("cohere", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        let text = read.text();
        assert!(!text.contains("event:"), "{text}");
        assert!(!text.contains("chat.completion.chunk"), "{text}");
        let last = last_payload(&text);
        assert_eq!(last["type"], "message-end", "{last}");
        assert!(
            last["delta"]["finish_reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("ERROR")),
            "{last}"
        );
        assert!(last.get("message").is_none(), "{last}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_responses_ingress_mid_stream_transport_error_appends_response_failed`

    #[test]
    fn test_responses_ingress_mid_stream_transport_error_appends_response_failed() {
        let (arrived, lane) = ask("responses", "openai", true);
        let ctx = reply_ctx(&arrived, &lane, false);
        let far = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let read = answer(&ctx, 200, SSE_HEAD, &[far], true);
        assert_eq!(read.status, 200);
        let text = read.text();
        assert!(text.contains("event: response.failed"), "{text}");
        let (_, data) = sse_frames(&text)
            .into_iter()
            .find(|(event, _)| event == "response.failed")
            .unwrap_or_else(|| panic!("a response.failed frame: {text}"));
        let failed: Value = serde_json::from_str(&data).expect("JSON");
        assert_eq!(failed["response"]["status"], "failed", "{failed}");
        assert!(
            failed["response"]["error"]["message"].is_string(),
            "{failed}"
        );
        assert!(failed.get("error").is_none(), "{failed}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_mid_stream_generic_detail_has_no_leak_markers`

    #[test]
    fn test_mid_stream_generic_detail_has_no_leak_markers() {
        const LEAK_MARKERS: &[&str] = &[
            "http://",
            "https://",
            "reqwest",
            "hyper",
            "tcp",
            "dns",
            "connect",
            "amazonaws",
            "url",
            "error sending request",
            "upstream",
            "proxy",
            "gateway",
            "backend",
            "lane",
            "translat",
        ];
        for detail in [
            wire::MID_STREAM_GENERIC_DETAIL,
            wire::GENERIC_REJECTED_DETAIL,
            wire::GENERIC_RESPONSE_ERROR_DETAIL,
        ] {
            for marker in LEAK_MARKERS {
                assert!(
                    !detail.to_ascii_lowercase().contains(marker),
                    "{marker:?} in {detail:?}"
                );
            }
        }
        let mut framer = busbar_plane_llm::codec::DECLS
            .iter()
            .find(|decl| decl.name == "gemini")
            .and_then(|decl| decl.dialect())
            .and_then(|codec| codec.make_array_stream_framer())
            .expect("gemini frames an array stream");
        let closed = framer.finish_with_server_error(wire::MID_STREAM_GENERIC_DETAIL);
        let closed = String::from_utf8_lossy(&closed);
        assert!(closed.contains(wire::MID_STREAM_GENERIC_DETAIL), "{closed}");
        for marker in ["https://", "reqwest", "hyper", "amazonaws"] {
            assert!(!closed.contains(marker), "{marker}: {closed}");
        }
        for dialect in ["openai", "anthropic", "gemini", "responses"] {
            let bytes = wire::mid_stream_error_bytes(
                HOST,
                dialect,
                false,
                wire::MID_STREAM_GENERIC_DETAIL,
                None,
            );
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains(wire::MID_STREAM_GENERIC_DETAIL),
                "{dialect}: {text}"
            );
        }
        let bytes = wire::mid_stream_error_bytes(
            HOST,
            "cohere",
            false,
            wire::MID_STREAM_GENERIC_DETAIL,
            None,
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("message-end") && text.contains("ERROR"),
            "{text}"
        );
        assert!(!text.contains(wire::MID_STREAM_GENERIC_DETAIL), "{text}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_bedrock_ingress_mid_stream_error_is_binary_exception_frame`

    #[test]
    fn test_bedrock_ingress_mid_stream_error_is_binary_exception_frame() {
        let crc = crc32fast::hash;
        let bytes =
            wire::mid_stream_error_bytes(HOST, "bedrock", true, "connection reset by peer", None);
        assert!(!bytes.starts_with(b"event:") && !bytes.starts_with(b"data:"));
        let word = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let total = word(0) as usize;
        assert_eq!(total, bytes.len(), "the total length");
        assert_eq!(word(8), crc(&bytes[..8]), "the prelude CRC");
        let len = bytes.len();
        assert_eq!(word(len - 4), crc(&bytes[..len - 4]), "the message CRC");
        let headers_len = word(4) as usize;
        let headers = String::from_utf8_lossy(&bytes[12..12 + headers_len]);
        for needle in [
            ":message-type",
            "exception",
            ":exception-type",
            "internalServerException",
        ] {
            assert!(headers.contains(needle), "{needle}: {headers}");
        }
        let payload: Value =
            serde_json::from_slice(&bytes[12 + headers_len..len - 4]).expect("a JSON payload");
        assert_eq!(payload["message"], "connection reset by peer");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_sse_ingress_mid_stream_error_uses_native_framing`

    #[test]
    fn test_sse_ingress_mid_stream_error_uses_native_framing() {
        for dialect in ["openai", "cohere", "gemini"] {
            let text = String::from_utf8(wire::mid_stream_error_bytes(
                HOST, dialect, false, "boom", None,
            ))
            .expect("text");
            assert!(text.starts_with("data: "), "{dialect}: {text}");
            assert!(!text.contains("event:"), "{dialect}: {text}");
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("a data line");
            let frame: Value = serde_json::from_str(data).expect("JSON");
            let cohere_end = frame["type"] == "message-end"
                && frame["delta"]["finish_reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("ERROR"));
            assert!(
                frame.get("error").is_some() || frame.get("message").is_some() || cohere_end,
                "{dialect}: {frame}"
            );
        }
        let text = String::from_utf8(wire::mid_stream_error_bytes(
            HOST,
            "anthropic",
            false,
            "boom",
            None,
        ))
        .expect("text");
        assert!(text.starts_with("event: error\n"), "{text}");
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("a data line");
        let frame: Value = serde_json::from_str(data).expect("JSON");
        assert!(frame["error"]["message"].is_string(), "{frame}");

        let text = String::from_utf8(wire::mid_stream_error_bytes(
            HOST,
            "responses",
            false,
            "boom",
            None,
        ))
        .expect("text");
        assert!(text.starts_with("event: response.failed\n"), "{text}");
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("a data line");
        let frame: Value = serde_json::from_str(data).expect("JSON");
        assert_eq!(frame["response"]["status"], "failed", "{frame}");
        assert!(frame["response"]["error"]["message"].is_string(), "{frame}");
        assert!(frame.get("error").is_none(), "{frame}");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_client_fault_kind_mapping`

    #[test]
    fn test_client_fault_kind_mapping() {
        assert_eq!(
            wire::client_fault_kind(StatusClass::ContextLength),
            "context_length_exceeded"
        );
        assert_eq!(
            wire::client_fault_kind(StatusClass::ClientError),
            "invalid_request_error"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_extract_error_message`

    #[test]
    fn test_extract_error_message() {
        assert_eq!(
            wire::extract_error_message(br#"{"error":{"message":"bad param"}}"#).as_deref(),
            Some("bad param")
        );
        assert_eq!(
            wire::extract_error_message(br#"{"message":"flat"}"#).as_deref(),
            Some("flat")
        );
        assert_eq!(wire::extract_error_message(b"not json"), None);
        assert_eq!(wire::extract_error_message(br#"{"foo":1}"#), None);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/mid_stream_error_tests.rs::test_is_stream_content_type`

    #[test]
    fn test_is_stream_content_type() {
        assert!(wire::is_stream_content_type("text/event-stream"));
        assert!(wire::is_stream_content_type(EVENTSTREAM));
        assert!(wire::is_stream_content_type(
            "text/event-stream; charset=utf-8"
        ));
        assert!(!wire::is_stream_content_type("application/json"));
        assert!(!wire::is_stream_content_type(""));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/wire_tests.rs::unknown_ingress_mid_stream_error_is_a_bare_data_frame_from_core`

    #[test]
    fn unknown_ingress_mid_stream_error_is_a_bare_data_frame_from_core() {
        let text = String::from_utf8(wire::mid_stream_error_bytes(
            HOST,
            "no-such-protocol",
            false,
            "upstream vanished",
            None,
        ))
        .expect("text");
        assert!(
            text.starts_with("data: ") && text.ends_with("\n\n"),
            "{text}"
        );
        assert!(!text.contains("event:"), "{text}");
        let frame: Value =
            serde_json::from_str(text.trim_start_matches("data: ").trim_end_matches("\n\n"))
                .expect("JSON");
        assert_eq!(frame["error"]["type"], "api_error", "{frame}");
        assert_eq!(frame["error"]["message"], "upstream vanished", "{frame}");
        assert_eq!(
            text,
            String::from_utf8_lossy(&wire::agnostic_stream_error_frame("upstream vanished"))
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/wire_tests.rs::status_word_golden_engine_client_fault_kind`

    #[test]
    fn status_word_golden_engine_client_fault_kind() {
        let golden: [(StatusClass, &str); 9] = [
            (StatusClass::RateLimit, "invalid_request_error"),
            (StatusClass::Overloaded, "invalid_request_error"),
            (StatusClass::ServerError, "invalid_request_error"),
            (StatusClass::Timeout, "invalid_request_error"),
            (StatusClass::Network, "invalid_request_error"),
            (StatusClass::Auth, "invalid_request_error"),
            (StatusClass::Billing, "invalid_request_error"),
            (StatusClass::ClientError, "invalid_request_error"),
            (StatusClass::ContextLength, "context_length_exceeded"),
        ];
        for (class, word) in golden {
            assert_eq!(wire::client_fault_kind(class), word, "{class:?}");
        }
    }

    /// The breaker's class rule (`busbar-kernel-breaker` `classify::normalize_raw_error`, 1.5.5's),

    fn breaker_rule(
        raw: &RawUpstreamError,
        error_map: &HashMap<String, String>,
    ) -> CanonicalSignal {
        let masks = |class: StatusClass| {
            class == StatusClass::ContextLength && (500..600).contains(&raw.http_status)
        };
        let signal = |class, provider_signal| CanonicalSignal {
            class,
            provider_signal,
            retry_after: raw.retry_after_secs,
        };
        let provider_signal = match &raw.provider_code {
            Some(code) => {
                if let Some(class) = error_map
                    .get(code)
                    .and_then(|mapped| StatusClass::parse(mapped))
                {
                    if !masks(class) {
                        return signal(class, Some(code.clone()));
                    }
                }
                if code == "context_length_exceeded"
                    && (raw.http_status == 400 || raw.http_status == 413)
                {
                    return signal(StatusClass::ContextLength, Some(code.clone()));
                }
                Some(code.clone())
            }
            None => None,
        };
        if let Some(ty) = &raw.structured_type {
            if let Some(class) = error_map
                .get(ty)
                .and_then(|mapped| StatusClass::parse(mapped))
            {
                if !masks(class) {
                    return signal(class, provider_signal.or_else(|| Some(ty.clone())));
                }
            }
        }
        let class = match raw.http_status {
            401 | 403 => StatusClass::Auth,
            429 => StatusClass::RateLimit,
            408 => StatusClass::Timeout,
            529 => StatusClass::Overloaded,
            500..=599 => StatusClass::ServerError,
            0 => StatusClass::Network,

            _ => StatusClass::ClientError,
        };
        signal(class, provider_signal)
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/plane_reply_parity_tests.rs::the_planes_class_rule_is_the_breakers`

    #[test]
    fn the_planes_class_rule_is_the_breakers() {
        let maps: Vec<HashMap<String, String>> = vec![
            HashMap::new(),
            map(&[
                ("1302", "rate_limit"),
                ("quota", "billing"),
                ("ctx", "context_length"),
                ("typo", "rate_limt"),
                ("overloaded_error", "overloaded"),
                ("invalid_request_error", "context_length"),
                ("auth", "auth"),
                ("slow", "timeout"),
                ("net", "network"),
                ("bad", "client_error"),
                ("boom", "server_error"),
            ]),
        ];
        let codes = [
            None,
            Some("1302"),
            Some("quota"),
            Some("ctx"),
            Some("typo"),
            Some("context_length_exceeded"),
            Some("auth"),
            Some("unmapped"),
        ];
        let types = [
            None,
            Some("overloaded_error"),
            Some("invalid_request_error"),
            Some("slow"),
            Some("net"),
            Some("bad"),
            Some("boom"),
            Some("unmapped"),
        ];
        let statuses = [
            200, 302, 400, 401, 403, 404, 408, 413, 422, 429, 500, 502, 503, 504, 529,
        ];
        let mut checked = 0;
        for error_map in &maps {
            for code in codes {
                for ty in types {
                    for status in statuses {
                        for retry in [None, Some(7)] {
                            let mut error = raw(status, code, ty);
                            error.retry_after_secs = retry;
                            assert_eq!(
                                normalize(status, &error, error_map),
                                breaker_rule(&error, error_map),
                                "{error:?} under {error_map:?}"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 2 * 8 * 8 * 15 * 2);
    }

    /// The far head every legacy parity fixture answered with.
    const PARITY_HEAD: &[(&[u8], &[u8])] = &[
        (b"content-type", b"application/json"),
        (b"request-id", b"req_far0001"),
        (b"x-amzn-requestid", b"amzn-far-0001"),
    ];

    /// REHOMES `crates/busbar-llm/src/engine/tests/plane_reply_parity_tests.rs::a_far_end_error_reads_the_same_through_the_plane`

    #[test]
    fn a_far_end_error_reads_the_same_through_the_plane() {
        let fixtures: [(u16, &[u8], Option<&str>); 3] = [
            (
                400,
                br#"{"error":{"message":"max_tokens is too large","type":"invalid_request_error"}}"#,
                Some("max_tokens is too large"),
            ),
            (422, br#"{"message":"bad field"}"#, Some("bad field")),
            (
                401,
                br#"{"error":{"message":"invalid x-api-key","type":"authentication_error"}}"#,
                None,
            ),
        ];
        let mut checked = 0;
        for far in SIX {
            for caller in SIX {
                let (arrived, lane) = ask(caller, far, false);
                let ctx = reply_ctx(&arrived, &lane, false);
                for (status, body, message) in fixtures {
                    let read = answer(&ctx, status, PARITY_HEAD, &[body], false);
                    let what = format!("{caller}<-{far} {status}");
                    assert_eq!(
                        read.value("content-length").as_deref(),
                        Some(read.body.len().to_string().as_str()),
                        "{what}"
                    );
                    match message {
                        Some(_) if caller == far => {
                            assert_eq!(read.status, status, "{what}");
                            assert_eq!(read.body, body, "{what}: verbatim");
                            assert_eq!(
                                read.value("request-id").as_deref(),
                                Some("req_far0001"),
                                "{what}"
                            );
                            assert_eq!(
                                read.value("x-amzn-requestid").as_deref(),
                                Some("amzn-far-0001"),
                                "{what}"
                            );
                        }
                        Some(message) => {
                            assert_eq!(read.status, status, "{what}");
                            assert_eq!(
                                without_minted_id(read.json()),
                                without_minted_id(own_envelope(
                                    caller,
                                    status,
                                    "invalid_request_error",
                                    message
                                )),
                                "{what}"
                            );
                            assert!(
                                read.fields.iter().all(|(_, value)| value != b"req_far0001"
                                    && value != b"amzn-far-0001"),
                                "{what}: {:?}",
                                read.fields
                            );
                        }
                        None => {
                            let decl = busbar_plane_llm::codec::DECLS
                                .iter()
                                .find(|decl| decl.name == caller)
                                .expect("a dialect");
                            let (own_status, kind) = decl.auth_failure_status_and_kind;
                            assert_eq!(read.status, own_status, "{what}");
                            assert_eq!(
                                without_minted_id(read.json()),
                                without_minted_id(own_envelope(
                                    caller,
                                    own_status,
                                    kind,
                                    decl.auth_failure_message
                                )),
                                "{what}"
                            );

                            assert_ne!(read.body, body, "{what}: never the far body");
                            if decl.auth_failure_message != "invalid x-api-key" {
                                assert!(!read.text().contains("invalid x-api-key"), "{what}");
                            }
                        }
                    }
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 3 * 36);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/plane_reply_parity_tests.rs::a_far_end_errors_relay_is_the_engines`

    #[test]
    fn a_far_end_errors_relay_is_the_engines() {
        let body = br#"{"error":{"message":"slow down"}}"#;
        let mut checked = 0;
        for (status, kind) in [
            (429u16, "rate_limit_error"),
            (500, "api_error"),
            (503, "overloaded"),
            (504, "timeout"),
            (529, "api_error"),
        ] {
            for far in SIX {
                for caller in SIX {
                    let far_error = FarError {
                        status,
                        head: JSON_HEAD,
                        body,
                    };
                    let relayed = failure::relay(HOST, caller, far, &far_error);
                    assert_eq!(relayed.status, status, "{caller}<-{far} {status}");
                    if caller == far {
                        assert_eq!(relayed.body, body, "{caller}: verbatim");
                    } else {
                        assert_eq!(
                            field(&relayed.fields, "content-type"),
                            Some(b"application/json".as_slice()),
                            "{caller}<-{far} {status}"
                        );
                        let envelope: Value = serde_json::from_slice(&relayed.body).expect("JSON");
                        assert_eq!(
                            without_minted_id(envelope),
                            without_minted_id(own_envelope(caller, status, kind, "slow down")),
                            "{caller}<-{far} {status}"
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 36);
    }
}

/// The far end's one content chunk before its connection dropped (an OpenAI chat stream), as the

const CUT_FIRST: &[u8] = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"},\"finish_reason\":null}]}\n\n";

/// THE BYTES 1.5.5 ENDED A CUT STREAM WITH, per caller dialect: recorded 2026-10-13 from the

const CUT_1_5_5: &[(&str, bool, &[u8])] = &[
    ("openai", false, b"data: {\"error\":{\"code\":null,\"message\":\"The response stream was interrupted.\",\"param\":null,\"type\":\"server_error\"}}\n\n"),
    ("anthropic", false, b"event: error\ndata: {\"error\":{\"message\":\"The response stream was interrupted.\",\"type\":\"api_error\"},\"type\":\"error\"}\n\n"),
    ("gemini", false, b"data: {\"error\":{\"code\":500,\"message\":\"The response stream was interrupted.\",\"status\":\"INTERNAL\"}}\n\n"),
    ("gemini", true, b",{\"error\":{\"code\":500,\"message\":\"The response stream was interrupted.\",\"status\":\"INTERNAL\"}}]"),
    ("cohere", false, b"data: {\"delta\":{\"finish_reason\":\"ERROR\",\"usage\":{\"tokens\":{\"input_tokens\":0,\"output_tokens\":0}}},\"type\":\"message-end\"}\n\n"),
    ("responses", false, b"event: response.failed\ndata: {\"response\":{\"created_at\":1791927860,\"error\":{\"code\":\"server_error\",\"message\":\"The response stream was interrupted.\"},\"id\":\"resp_zjjqukHWABl78LV1C9NNR22oHbFmGEmq0TypczYXR4ZIVu2O\",\"model\":\"gpt-4o\",\"object\":\"response\",\"output\":[],\"status\":\"failed\"},\"sequence_number\":0,\"type\":\"response.failed\"}\n\n"),
];

/// The Bedrock caller (1.5.5 sent `InternalServerException`; the union member is `internalServerException`): one exception frame (binary eventstream).
const CUT_1_5_5_BEDROCK: &str = "000000a7000000659afd09380f3a657863657074696f6e2d74797065070017696e7465726e616c536572766572457863657074696f6e0d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e0d3a6d6573736167652d74797065070009657863657074696f6e7b226d657373616765223a2254686520726573706f6e73652073747265616d2077617320696e7465727275707465642e227de769a380";

/// The members of a Response object signed register entry S-6 adds to 1.5.5's (the spec's required

const S6_RESPONSE_MEMBERS: &[&str] = &[
    "incomplete_details",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "metadata",
    "temperature",
    "top_p",
];

/// A Responses `response.failed` frame as 1.5.5's recording is compared to it: its event name and

fn without_run_ids(frame: &[u8]) -> (Vec<u8>, Value) {
    let text = std::str::from_utf8(frame).expect("text");
    let (event, data) = text.split_once("\ndata: ").expect("an event and its data");
    let mut v: Value = serde_json::from_str(data.trim_end()).expect("JSON");
    if let Some(top) = v.as_object_mut() {
        top.remove("sequence_number");
    }
    if let Some(r) = v.get_mut("response").and_then(Value::as_object_mut) {
        r.remove("id");
        r.remove("created_at");
        r.remove("model");
        for member in S6_RESPONSE_MEMBERS {
            r.remove(*member);
        }
    }
    (event.as_bytes().to_vec(), v)
}

/// The run's own values on a candidate `response.failed` frame are there and well formed.
fn assert_run_ids(frame: &[u8]) {
    let text = std::str::from_utf8(frame).expect("text");
    let (_, data) = text.split_once("\ndata: ").expect("an event and its data");
    let v: Value = serde_json::from_str(data.trim_end()).expect("JSON");
    assert!(v["sequence_number"].is_u64(), "S-2 numbers the frame: {v}");
    let r = &v["response"];
    assert!(
        r["id"].as_str().is_some_and(|id| id.starts_with("resp_")),
        "{v}"
    );
    assert!(r["created_at"].is_u64(), "{v}");
    assert!(r["model"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
    for member in S6_RESPONSE_MEMBERS {
        assert!(r.get(*member).is_some(), "S-6 member {member}: {v}");
    }
}

/// A STREAM CUT AFTER ITS FIRST BYTE ENDS IN 1.5.5'S BYTES, for every caller dialect: the in-band error 1.5.5 wrote, in

#[test]
fn a_stream_cut_after_its_first_byte_ends_in_1_5_5s_bytes_for_every_caller_dialect() {
    let hex = |s: &str| -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    };
    let bedrock = hex(CUT_1_5_5_BEDROCK);
    let cases = CUT_1_5_5
        .iter()
        .copied()
        .chain(std::iter::once(("bedrock", false, &bedrock[..])));
    for (ingress, json_array, want) in cases {
        let mut ctx = relay_ctx(ingress, "openai", true);
        ctx.json_array = json_array;
        let mut r = Relay::new(ctx);
        let _ = r.feed(HOST, CUT_FIRST);
        let cut = r.cut(HOST, true);
        assert_eq!(cut.reason, "mid-stream", "{ingress}");
        let mut got = cut.bytes.expect("the caller's in-band error");
        if ingress == "anthropic" {
            let text = String::from_utf8(got).expect("text");
            let at = text
                .find(r#""request_id":"req_"#)
                .expect("R-22's request_id");
            let end = at + text[at..].find("\",").expect("the request_id ends") + 2;
            got = format!("{}{}", &text[..at], &text[end..]).into_bytes();
        }
        if ingress == "responses" {
            assert_run_ids(&got);
            assert_eq!(without_run_ids(&got), without_run_ids(want), "{ingress}");
        } else {
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(want),
                "{ingress} (json array: {json_array})"
            );
            assert_eq!(got, want, "{ingress} (json array: {json_array})");
        }
    }
}

/// A RESPONSES STREAM THE FAR END ENDS AS FAILED BILLS NO TOKENS (`$`: 1.5.5 billed nothing for a stream with a terminal error, v1.5.5

#[test]
fn a_responses_stream_that_fails_bills_no_tokens_and_agrees_with_its_buffered_twin() {
    let mut arrived = arrival("responses", true);
    arrived.path = "/v1/responses".to_string();
    let body = serde_json::json!({"model": "p", "stream": true, "input": "hi"});
    arrived.body = serde_json::to_vec(&body).unwrap();
    arrived.parsed = Some(body);
    let lane = lane("responses");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("responses"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"m-1\",\"output\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hel\"}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"failed\",\"model\":\"m-1\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"boom\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n",
    );
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let end = reply.feed(HOST, &ctx, stream.as_bytes(), true, AT);
    assert!(end.done, "{end:?}");
    assert_eq!(
        (end.units.tokens_in, end.units.tokens_out),
        (0, 0),
        "a failed stream bills no tokens: {:?}",
        end.units
    );

    let arrived = {
        let mut a = arrival("responses", false);
        a.path = "/v1/responses".to_string();
        let body = serde_json::json!({"model": "p", "input": "hi"});
        a.body = serde_json::to_vec(&body).unwrap();
        a.parsed = Some(body);
        a
    };
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("responses"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let failed = br#"{"id":"resp_1","object":"response","created_at":1,"status":"failed","model":"m-1","output":[],"error":{"code":"server_error","message":"boom"},"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}"#;
    let mut reply = Reply::new(HOST, &ctx, 200, JSON_HEAD);
    let whole = reply.feed(HOST, &ctx, failed, true, AT);
    assert_eq!(
        (whole.units.tokens_in, whole.units.tokens_out),
        (0, 0),
        "the buffered twin agrees: {:?}",
        whole.units
    );
}

/// A top-level upstream error keeps native frames byte-identical while marking the exchange

#[test]
fn a_responses_streams_top_level_error_event_fails_the_exchange_without_changing_native_bytes() {
    let mut arrived = arrival("responses", true);
    arrived.path = "/v1/responses".to_string();
    let body = serde_json::json!({"model": "p", "stream": true, "input": "hi"});
    arrived.body = serde_json::to_vec(&body).unwrap();
    arrived.parsed = Some(body);
    let lane = lane("responses");
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: stream_intent(chat_handler("responses"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"m-1\",\"output\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hel\"}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"sequence_number\":2,\"code\":\"server_error\",\"message\":\"boom\",\"param\":null}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"completed\",\"model\":\"m-1\",\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n",
    );
    let head: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    let mut reply = Reply::new(HOST, &ctx, 200, head);
    let end = reply.feed(HOST, &ctx, stream.as_bytes(), true, AT);
    assert!(end.done, "{end:?}");
    assert_eq!(end.fault, Some(Fault::Transient("stream-terminal-error")));
    assert_eq!(end.verdict, Verdict::Hard);
    assert_eq!(end.bytes.as_ref(), stream.as_bytes());
    assert_eq!((end.units.tokens_in, end.units.tokens_out), (0, 0));
}

/// The legacy engine's tests that pin what an answer bills (`crates/busbar-llm/src/engine/**`),

mod rehomed_metering {
    use super::*;
    use std::borrow::Cow;

    use busbar_contract::abi::transport::{FAULT_NONE, FAULT_TRANSIENT};
    use busbar_plane_llm::exchange::arrive::PathModel;
    use busbar_plane_llm::exchange::attempt::StreamIntent;
    use busbar_plane_llm::exchange::reply::{Piece, Units};
    use busbar_plane_llm::plane_door::breaker_fault;
    use serde_json::json;

    const SSE_HEAD: &[(&[u8], &[u8])] = &[(b"content-type", b"text/event-stream")];
    const EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

    /// A chat request in `dialect`'s own shape.
    fn chat_request(dialect: &str, stream: bool) -> Value {
        match dialect {
            "anthropic" => json!({"model": "p", "max_tokens": 64, "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]}),
            "responses" => json!({"model": "p", "stream": stream, "input": "hi"}),
            "gemini" => json!({"model": "p", "stream": stream,
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
            "bedrock" => json!({"model": "p", "stream": stream,
                "messages": [{"role": "user", "content": [{"text": "hi"}]}]}),
            _ => json!({"model": "p", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]}),
        }
    }

    /// The caller's arrival of `request` for `operation` in `dialect` (`None`: a body that is not

    fn arrived_for(
        dialect: &'static str,
        operation: OpVerb,
        request: Option<Value>,
        json_array: bool,
    ) -> Arrived {
        let stream = request
            .as_ref()
            .and_then(|body| body["stream"].as_bool())
            .unwrap_or(false);
        Arrived {
            dialect,
            operation,
            model: "p".to_string(),
            content_type: "application/json".to_string(),
            body: request
                .as_ref()
                .map(|body| serde_json::to_vec(body).expect("JSON"))
                .unwrap_or_default(),
            parsed: request,
            path: String::new(),
            query: None,
            path_model: matches!(dialect, "gemini" | "bedrock").then_some(PathModel {
                stream,
                json_array,
                model_not_found_message: None,
            }),
        }
    }

    /// A `caller` asking a far end of `far` for a chat answer, streamed when `stream`.
    fn ask(caller: &'static str, far: &'static str, stream: bool) -> (Arrived, Lane) {
        (
            arrived_for(
                caller,
                OpVerb::CHAT,
                Some(chat_request(caller, stream)),
                false,
            ),
            lane(far),
        )
    }

    fn reply_ctx<'a>(arrived: &'a Arrived, lane: &'a Lane) -> ReplyCtx<'a> {
        reply_ctx_capped(
            arrived,
            lane,
            busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
        )
    }

    /// The reply's context under the translation cap `translate_cap` the plane's settings carry.
    fn reply_ctx_capped<'a>(
        arrived: &'a Arrived,
        lane: &'a Lane,
        translate_cap: usize,
    ) -> ReplyCtx<'a> {
        let wants_stream = arrived
            .parsed
            .as_ref()
            .and_then(|request| request["stream"].as_bool())
            .unwrap_or(false);
        ReplyCtx {
            arrived,
            lane,
            intent: StreamIntent {
                wants_stream,
                client_include_usage: false,
            },
            passthrough: false,
            translate_cap,
        }
    }

    /// A piece the caller read, its bytes its own.
    fn owned(piece: Piece<'_>) -> Piece<'static> {
        Piece {
            head: piece.head,
            bytes: Cow::Owned(piece.bytes.into_owned()),
            units: piece.units,
            verdict: piece.verdict,
            fault: piece.fault,
            tap_fault: piece.tap_fault,
            done: piece.done,
            dropped: piece.dropped,
            synthesized: piece.synthesized,
        }
    }

    /// What the caller read of one answer.
    struct Read {
        status: u16,
        fields: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
        pieces: Vec<Piece<'static>>,
    }

    impl Read {
        /// The piece that ended the answer.
        fn end(&self) -> &Piece<'static> {
            self.pieces
                .iter()
                .find(|piece| piece.done)
                .or(self.pieces.last())
                .expect("a piece")
        }

        /// The far end's reported (input, output) tokens the answer settled on.
        fn tokens(&self) -> (u64, u64) {
            (self.end().units.tokens_in, self.end().units.tokens_out)
        }

        /// How many pieces ended the answer: the one settlement.
        fn settlements(&self) -> usize {
            self.pieces.iter().filter(|piece| piece.done).count()
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|err| panic!("a JSON body ({err}): {}", self.text()))
        }

        /// The first head field named `name`, as text.
        fn value(&self, name: &str) -> Option<String> {
            field(&self.fields, name).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        }
    }

    /// One answer through the served reply: the far end's `status` and `head`, its body in

    fn answer(
        ctx: &ReplyCtx<'_>,
        status: u16,
        head: &[(&[u8], &[u8])],
        pieces: &[&[u8]],
        cut: bool,
    ) -> Read {
        let mut reply = Reply::new(HOST, ctx, status, head);
        let mut out: Vec<Piece<'static>> = Vec::new();
        for (index, bytes) in pieces.iter().enumerate() {
            let last = index + 1 == pieces.len() && !cut;
            out.push(owned(reply.feed(HOST, ctx, bytes, last, AT)));
        }
        if pieces.is_empty() {
            out.push(owned(reply.feed(HOST, ctx, &[], !cut, AT)));
        }
        if cut {
            out.push(reply.cut(HOST, ctx, true));
        }
        let mut read = Read {
            status: 0,
            fields: Vec::new(),
            body: Vec::new(),
            pieces: Vec::new(),
        };
        for piece in &out {
            if let (Some(head), 0) = (&piece.head, read.status) {
                read.status = head.status;
                read.fields = head.fields.clone();
            }
            read.body.extend_from_slice(&piece.bytes);
        }
        read.pieces = out;
        read
    }

    /// `events` framed as a far end's `data:` stream.
    fn data_frames(events: &[&str]) -> Vec<u8> {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            .into_bytes()
    }

    /// A cohere whole chat body with `finish_reason`, reporting 10 in / 5 out.
    fn cohere_body(finish_reason: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "c-1",
            "finish_reason": finish_reason,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
            "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
        }))
        .expect("JSON")
    }

    /// A gemini event stream: a text chunk, then the terminal chunk with `finish_reason`,

    fn gemini_stream(finish_reason: &str) -> Vec<u8> {
        let first = json!({"candidates": [{"index": 0,
            "content": {"role": "model", "parts": [{"text": "x"}]}}]});
        let last = json!({"candidates": [{"index": 0,
            "content": {"role": "model", "parts": [{"text": ""}]},
            "finishReason": finish_reason}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5,
                "totalTokenCount": 15}});
        data_frames(&[&first.to_string(), &last.to_string()])
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::buffered_cohere_error_is_an_error_a_breaker_fault_and_charges_10_5`

    #[test]
    fn buffered_cohere_error_is_an_error_a_breaker_fault_and_charges_10_5() {
        let (arrived, lane) = ask("openai", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = cohere_body("ERROR");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 502, "{}", read.text());
        assert!(
            !read.text().contains("\"finish_reason\""),
            "{}",
            read.text()
        );
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(
            read.end().fault,
            Some(Fault::Transient("upstream-generation-failed"))
        );
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
        assert_eq!(read.tokens(), (10, 5));
        assert_eq!(read.tokens().0 + read.tokens().1, 15);
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::buffered_cohere_complete_is_a_success_and_charges_10_5`

    #[test]
    fn buffered_cohere_complete_is_a_success_and_charges_10_5() {
        let (arrived, lane) = ask("openai", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = cohere_body("COMPLETE");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200, "{}", read.text());
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.end().fault, None);
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_NONE);
        assert_eq!(read.tokens(), (10, 5));
        assert_eq!(read.tokens().0 + read.tokens().1, 15);
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::same_protocol_cohere_error_relays_verbatim_faults_the_breaker_and_charges_10_5`

    #[test]
    fn same_protocol_cohere_error_relays_verbatim_faults_the_breaker_and_charges_10_5() {
        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = cohere_body("ERROR");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.body, body, "the far end's bytes");
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(
            read.end().fault,
            Some(Fault::Transient("upstream-generation-failed"))
        );
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
        assert_eq!(read.tokens(), (10, 5));
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::same_protocol_cohere_complete_is_a_success_and_charges_10_5`

    #[test]
    fn same_protocol_cohere_complete_is_a_success_and_charges_10_5() {
        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = cohere_body("COMPLETE");
        let read = answer(&ctx, 200, JSON_HEAD, &[&body], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.end().fault, None);
        assert_eq!(read.tokens(), (10, 5));
        assert_eq!(read.tokens().0 + read.tokens().1, 15);
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::streamed_gemini_malformed_function_call_is_an_error_frame_a_breaker_fault_and_charges`

    #[test]
    fn streamed_gemini_malformed_function_call_is_an_error_frame_a_breaker_fault_and_charges() {
        let (arrived, lane) = ask("openai", "gemini", true);
        let ctx = reply_ctx(&arrived, &lane);
        let far = gemini_stream("MALFORMED_FUNCTION_CALL");
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(read.text().contains("\"error\""), "{}", read.text());
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
        assert_eq!(read.tokens(), (10, 5));
        assert_eq!(read.tokens().0 + read.tokens().1, 15);
    }

    /// REHOMES `crates/busbar-llm/src/engine/attempt/tests/failed_generation.rs::streamed_gemini_stop_is_a_success_and_charges`

    #[test]
    fn streamed_gemini_stop_is_a_success_and_charges() {
        let (arrived, lane) = ask("openai", "gemini", true);
        let ctx = reply_ctx(&arrived, &lane);
        let far = gemini_stream("STOP");
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert!(!read.text().contains("\"error\""), "{}", read.text());
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.end().fault, None);
        assert_eq!(read.tokens(), (10, 5));
        assert_eq!(read.tokens().0 + read.tokens().1, 15);
    }

    /// REHOMES `crates/busbar-llm/src/engine/engine_tests/crossproto_delivery_billing_tests.rs::delivered_cross_protocol_response_bills_once`

    #[test]
    fn delivered_cross_protocol_response_bills_once() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = br#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":13,"completion_tokens":9}}"#;
        let (head, tail) = body.split_at(body.len() / 2);
        let read = answer(&ctx, 200, JSON_HEAD, &[head, tail], false);
        assert_eq!(read.status, 200);
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.end().fault, None);
        assert_eq!(read.tokens(), (13, 9));
        assert_eq!(read.tokens().0 + read.tokens().1, 22);
        assert_eq!(read.settlements(), 1, "billed once");
        assert_eq!(
            read.pieces[0].units,
            Units::default(),
            "nothing billed before the whole answer is read"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/engine_tests/crossproto_delivery_billing_tests.rs::ingress_unsupported_404_does_not_charge`

    #[test]
    fn ingress_unsupported_404_does_not_charge() {
        let body = br#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],"model":"text-embedding-3-small","usage":{"prompt_tokens":42}}"#;
        let request = json!({"model": "p", "input": "hi"});
        let arrived = arrived_for("anthropic", OpVerb::EMBEDDINGS, Some(request), false);
        let lane = lane("openai");
        let ctx = reply_ctx(&arrived, &lane);
        let read = answer(&ctx, 200, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 404, "{}", read.text());
        assert_eq!(read.end().units, Units::default(), "nothing billed");
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(read.end().fault, None);
        let mut whole_ctx = super::ctx("anthropic", "openai", false, false);
        whole_ctx.operation = OpVerb::EMBEDDINGS;
        let answered = whole::translate(HOST, &whole_ctx, 200, body);
        assert_eq!(answered.end, WholeEnd::IngressUnsupported);
        assert_eq!(answered.usage, None);
    }

    /// REHOMES `crates/busbar-llm/src/engine/engine_tests/crossproto_delivery_billing_tests.rs::untranslatable_500_does_not_charge`

    #[test]
    fn untranslatable_500_does_not_charge() {
        let body: &[u8] = &[
            0x00, 0x01, 0x02, 0xFF, b'n', b'o', b't', b'-', b'j', b's', b'o', b'n',
        ];
        let arrived = arrived_for("anthropic", OpVerb::SPEECH, None, false);
        let lane = lane("openai");
        let ctx = reply_ctx(&arrived, &lane);
        let read = answer(&ctx, 200, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 500, "{}", read.text());
        assert_eq!(read.end().units, Units::default(), "nothing billed");
        assert_eq!(read.end().verdict, Verdict::Hard);
    }

    /// A chat-dialect 2xx whose `choices` is empty: the chat reader refuses it.
    const EMPTY_CHOICES: &[u8] = br#"{"id":"chatcmpl-EMPTY","object":"chat.completion","created":1234567890,"model":"glm-4.5","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#;

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_once_pool_cell_tests.rs::test_forward_once_untranslatable_2xx_refunds_budget_and_trips_breaker`

    #[test]
    fn test_forward_once_untranslatable_2xx_refunds_budget_and_trips_breaker() {
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane);
        let read = answer(&ctx, 200, JSON_HEAD, &[EMPTY_CHOICES], false);
        assert_eq!(read.status, 500, "{}", read.text());
        assert_eq!(read.end().verdict, Verdict::Hard);
        assert_eq!(
            read.end().fault,
            Some(Fault::Transient("untranslatable-2xx"))
        );
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
        assert_eq!(read.end().units, Units::default());
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_untranslatable_2xx_refunds_budget_and_trips_breaker`

    #[test]
    fn test_untranslatable_2xx_refunds_budget_and_trips_breaker() {
        let answered = whole::translate(
            HOST,
            &super::ctx("anthropic", "openai", false, false),
            200,
            EMPTY_CHOICES,
        );
        assert_eq!(answered.end, WholeEnd::NotTranslatable);
        assert_eq!(answered.usage, None);
        assert_eq!(answered.answer.status, 500);
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx(&arrived, &lane);
        let read = answer(&ctx, 200, JSON_HEAD, &[EMPTY_CHOICES], false);
        assert_eq!(read.status, 500);
        assert_eq!(breaker_fault(read.end().fault.as_ref()), FAULT_TRANSIENT);
        assert_eq!(read.end().units, Units::default());
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/forward_pool_integration_tests.rs::test_openai_ingress_single_model_anthropic_response_translated`

    #[test]
    fn test_openai_ingress_single_model_anthropic_response_translated() {
        let (arrived, lane) = ask("openai", "anthropic", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":9,"output_tokens":3}}"#;
        let read = answer(&ctx, 200, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 200);
        let completion = read.json();
        assert_eq!(completion["object"], "chat.completion", "{completion}");
        assert_eq!(completion["choices"][0]["message"]["content"], "hi there");
        assert_eq!(completion["choices"][0]["finish_reason"], "stop");
        assert_eq!(completion["usage"]["prompt_tokens"], 9);
        assert_eq!(completion["usage"]["completion_tokens"], 3);
        assert!(completion.get("stop_reason").is_none(), "{completion}");
        assert_eq!(read.tokens(), (9, 3));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_same_protocol_nonstream_multichunk_counts_usage`

    #[test]
    fn test_same_protocol_nonstream_multichunk_counts_usage() {
        let (arrived, lane) = ask("openai", "openai", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = r#"{"id":"chatcmpl-split","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":600,"completion_tokens":400}}"#;
        let split = body.find(r#""usage""#).expect("a usage member");
        let read = answer(
            &ctx,
            200,
            JSON_HEAD,
            &[&body.as_bytes()[..split], &body.as_bytes()[split..]],
            false,
        );
        assert_eq!(read.body, body.as_bytes(), "byte-identical");
        assert_eq!(read.end().verdict, Verdict::Ok);
        assert_eq!(read.tokens(), (600, 400));
        assert_eq!(read.tokens().0 + read.tokens().1, 1000);
    }

    /// How many times the usage tap's copy of a same-dialect chat body, fed in 512-byte pieces under

    fn truncations(body: &[u8], cap: usize) -> usize {
        let mut ctx = super::relay_ctx("openai", "openai", false);
        ctx.translate_cap = cap;
        let mut relay = Relay::new(ctx);
        body.chunks(512)
            .filter(|piece| relay.feed(HOST, piece).1)
            .count()
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_same_protocol_nonstream_over_cap_body_bills_no_trailing_usage_as_in_1_5_5`

    #[test]
    fn a_same_dialect_body_past_the_cap_bills_no_trailing_usage_as_in_1_5_5() {
        const CAP: usize = 4096;
        let (arrived, lane) = ask("openai", "openai", false);
        let ctx = reply_ctx_capped(&arrived, &lane, CAP);
        let filler = "x".repeat(CAP * 3);
        let body = format!(
            r#"{{"id":"chatcmpl-huge","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"{filler}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":600,"completion_tokens":400}}}}"#
        );
        assert!(body.len() > CAP * 2);
        let pieces: Vec<&[u8]> = body.as_bytes().chunks(512).collect();
        let read = answer(&ctx, 200, JSON_HEAD, &pieces, false);
        assert_eq!(read.body, body.as_bytes(), "the whole body, verbatim");
        assert_eq!(read.tokens(), (0, 0), "no usage past the cap bills");
        assert_eq!(read.end().units, Units::default(), "nothing billed");
        assert_eq!(
            truncations(body.as_bytes(), CAP),
            1,
            "the overflow, counted once"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_truncated_beyond_recovery_bills_no_floor_as_in_1_5_5`

    #[test]
    fn a_same_dialect_body_truncated_past_the_cap_bills_no_floor_as_in_1_5_5() {
        const CAP: usize = 4096;
        let (arrived, lane) = ask("openai", "openai", false);
        let ctx = reply_ctx_capped(&arrived, &lane, CAP);
        let filler = "x".repeat(CAP * 3);
        let body = format!(
            r#"{{"id":"chatcmpl-huge","object":"chat.completion","usage":{{"prompt_tokens":600,"completion_tokens":400}},"choices":[{{"index":0,"message":{{"role":"assistant","content":"{filler}"}},"finish_reason":"stop"}}]}}"#
        );
        let pieces: Vec<&[u8]> = body.as_bytes().chunks(512).collect();
        let read = answer(&ctx, 200, JSON_HEAD, &pieces, false);
        assert_eq!(read.body, body.as_bytes(), "the whole body, verbatim");
        assert_eq!(read.tokens(), (0, 0), "no floor past the cap");
        assert_eq!(read.end().units, Units::default(), "nothing billed");
        assert_eq!(
            truncations(body.as_bytes(), CAP),
            1,
            "the overflow, counted once"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_stream_delivers_trailing_usage_gemini_json_array`

    #[test]
    fn test_cross_protocol_stream_delivers_trailing_usage_gemini_json_array() {
        let arrived = arrived_for(
            "gemini",
            OpVerb::CHAT,
            Some(chat_request("gemini", true)),
            true,
        );
        let lane = lane("openai");
        let ctx = reply_ctx(&arrived, &lane);
        let far = [
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":600,\"completion_tokens\":400}}\n\n",
            "data: [DONE]\n\n",
        ];
        let pieces: Vec<&[u8]> = far.iter().map(|frame| frame.as_bytes()).collect();
        let read = answer(&ctx, 200, SSE_HEAD, &pieces, false);
        let array = read.json();
        let prompt = array.as_array().and_then(|elements| {
            elements
                .iter()
                .find_map(|element| element["usageMetadata"]["promptTokenCount"].as_i64())
        });
        assert_eq!(prompt, Some(600), "{array}");
        assert_eq!(read.tokens(), (600, 400));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_stream_delivers_trailing_usage_anthropic_sse`.

    #[test]
    fn a_chat_streams_trailing_usage_reaches_an_anthropic_callers_terminal_message_delta() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let far = [
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":600,\"completion_tokens\":400}}\n\n",
            "data: [DONE]\n\n",
        ];
        let pieces: Vec<&[u8]> = far.iter().map(|frame| frame.as_bytes()).collect();
        let read = answer(&ctx, 200, SSE_HEAD, &pieces, false);
        let text = read.text();
        let output_tokens = text.split("\n\n").find_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let v: Value = serde_json::from_str(data).ok()?;
            (v["type"] == "message_delta").then(|| v["usage"]["output_tokens"].as_i64())?
        });
        assert_eq!(output_tokens, Some(400), "{text}");
        assert_eq!(read.tokens(), (600, 400));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_mid_stream_transport_error_does_not_bill_partial_usage`

    #[test]
    fn rehomed_mid_stream_transport_error_does_not_bill_partial_usage() {
        let (arrived, lane) = ask("openai", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane);
        let far = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":50}}\n\n",
        ];
        let pieces: Vec<&[u8]> = far.iter().map(|frame| frame.as_bytes()).collect();
        let read = answer(&ctx, 200, SSE_HEAD, &pieces, true);
        let cut = read.pieces.last().expect("the cut");
        assert!(
            cut.done && !cut.bytes.is_empty(),
            "the caller's error frame"
        );
        assert_eq!(cut.verdict, Verdict::Hard);
        assert_eq!(cut.fault, Some(Fault::Transient("mid-stream")));
        assert_eq!((cut.units.tokens_in, cut.units.tokens_out), (100, 50));
        assert_eq!(cut.units.tokens_in + cut.units.tokens_out, 150);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_bedrock_to_gemini_carries_total_tokens_and_response_id`

    #[test]
    fn test_cross_protocol_bedrock_to_gemini_carries_total_tokens_and_response_id() {
        let (arrived, lane) = ask("gemini", "bedrock", false);
        let ctx = reply_ctx(&arrived, &lane);
        let body = br#"{"output":{"message":{"role":"assistant","content":[{"text":"Hi"}]}},"stopReason":"end_turn","usage":{"inputTokens":7,"outputTokens":3}}"#;
        let read = answer(&ctx, 200, JSON_HEAD, &[body], false);
        assert_eq!(read.status, 200);
        assert_eq!(
            read.value("content-type").as_deref(),
            Some("application/json")
        );
        let answer_body = read.json();
        assert_eq!(
            answer_body["usageMetadata"]["totalTokenCount"],
            json!(10u64)
        );
        assert_eq!(
            answer_body["usageMetadata"]["promptTokenCount"],
            json!(7u64)
        );
        assert_eq!(
            answer_body["usageMetadata"]["candidatesTokenCount"],
            json!(3u64)
        );
        assert!(
            !answer_body["responseId"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{answer_body}"
        );
        let text = read.text();
        assert!(
            !text.contains("chatcmpl-") && !text.contains("msg_"),
            "{text}"
        );
        assert_eq!(read.tokens(), (7, 3));
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cross_protocol_nonstream_over_cap_body_returns_500_uncharged`

    #[test]
    fn rehomed_cross_protocol_nonstream_over_cap_body_returns_500_uncharged() {
        const CAP: usize = 4096;
        let (arrived, lane) = ask("anthropic", "openai", false);
        let ctx = reply_ctx_capped(&arrived, &lane, CAP);
        let huge = "x".repeat(CAP + 1024);
        let body = serde_json::to_vec(&json!({
            "id": "chatcmpl-huge", "object": "chat.completion", "created": 1_234_567_890,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": huge},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 999_999}
        }))
        .expect("JSON");
        let pieces: Vec<&[u8]> = body.chunks(512).collect();
        let read = answer(&ctx, 200, JSON_HEAD, &pieces, false);
        assert_eq!(read.status, 500, "{}", read.text());
        assert_eq!(read.settlements(), 1);
        let end = read.end();
        assert_eq!(end.verdict, Verdict::Hard);
        assert_eq!(end.fault, None, "our cap, not the far end's fault");
        assert_eq!(end.units, Units::default(), "nothing billed");
        assert!(
            read.pieces
                .iter()
                .all(|piece| piece.units == Units::default()),
            "no piece bills"
        );
    }

    /// A chat-dialect trailing usage chunk: 600 prompt / 400 completion.
    const USAGE_CHUNK: &[u8] =
        b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":600,\"completion_tokens\":400}}\n\n";

    /// More bytes than one event-stream frame may hold, with no frame end: the reframing gives up.
    fn overflow() -> Vec<u8> {
        vec![b'x'; busbar_plane_llm::codec::eventstream::MAX_FRAME_BYTES + 16]
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_streaming_translate_abort_trips_breaker_and_skips_billing`

    #[test]
    fn rehomed_streaming_translate_abort_trips_breaker_and_skips_billing() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let overflow = overflow();
        let read = answer(&ctx, 200, SSE_HEAD, &[USAGE_CHUNK, &overflow], false);
        let end = read.end();
        assert!(end.done);
        assert_eq!(end.verdict, Verdict::Hard);
        assert_eq!(end.fault, Some(Fault::Transient("stream-translate-abort")));
        assert_eq!(
            breaker_fault(end.fault.as_ref()),
            FAULT_TRANSIENT,
            "the far end's breaker reads a transient: the cell trips Open"
        );
        assert_eq!(
            read.tokens(),
            (600, 400),
            "an aborted stream bills what the far end reported before the abort (#62)"
        );
        assert_eq!(end.units.tokens_in + end.units.tokens_out, 1000);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cancel_drop_bills_partial_tokens`

    #[test]
    fn rehomed_cancel_drop_bills_partial_tokens() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let mut reply = Reply::new(HOST, &ctx, 200, SSE_HEAD);
        let piece = reply.feed(HOST, &ctx, USAGE_CHUNK, false, AT);
        assert!(!piece.done);
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (600, 400));
        assert_eq!(piece.units.tokens_in + piece.units.tokens_out, 1000);
        assert!(reply.partial(), "the caller had a usable part");
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/ingress_indistinguishability_tests.rs::test_cancel_drop_bills_streamed_tokens_on_aborted_translate`

    #[test]
    fn test_cancel_drop_bills_streamed_tokens_on_aborted_translate() {
        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let mut reply = Reply::new(HOST, &ctx, 200, SSE_HEAD);
        let first = reply.feed(HOST, &ctx, USAGE_CHUNK, false, AT);
        assert_eq!((first.units.tokens_in, first.units.tokens_out), (600, 400));
        let overflow = overflow();
        let after = reply.feed(HOST, &ctx, &overflow, false, AT);
        assert!(!after.done);
        assert_eq!((after.units.tokens_in, after.units.tokens_out), (600, 400));
        assert_eq!(after.units.tokens_in + after.units.tokens_out, 1000);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/plane_reply_parity_tests.rs::a_cut_stream_reads_the_same_through_the_plane`

    #[test]
    fn a_cut_stream_reads_the_same_through_the_plane() {
        use busbar_plane_llm::exchange::reply::wire;
        let mut checked = 0;
        for far in ["openai", "anthropic", "bedrock"] {
            let bytes = native_stream(far);
            let far_type = wire::ingress_stream_content_type(far).expect("a stream type");
            let head: &[(&[u8], &[u8])] = &[(b"content-type", far_type.as_bytes())];
            let mut pieces: Vec<&[u8]> = bytes.chunks(64).collect();
            pieces.truncate(pieces.len() / 2);
            for caller in SIX {
                let what = format!("{caller}<-{far}");
                let (arrived, lane) = ask(caller, far, true);
                let ctx = reply_ctx(&arrived, &lane);
                let read = answer(&ctx, 200, head, &pieces, true);
                assert_eq!(read.status, 200, "{what}");
                let caller_type = wire::ingress_stream_content_type(caller).expect("a type");
                assert_eq!(
                    read.value("content-type").as_deref(),
                    Some(caller_type),
                    "{what}"
                );
                let cut = read.pieces.last().expect("the cut");
                assert!(cut.done, "{what}");
                assert_eq!(cut.verdict, Verdict::Hard, "{what}");
                assert_eq!(cut.fault, Some(Fault::Transient("mid-stream")), "{what}");
                let before = &read.pieces[read.pieces.len() - 2].units;
                assert_eq!(
                    (cut.units.tokens_in, cut.units.tokens_out),
                    (before.tokens_in, before.tokens_out),
                    "{what}: the cut settles what streamed"
                );
                if far == "openai" {
                    assert_eq!(
                        (cut.units.tokens_in, cut.units.tokens_out),
                        (0, 0),
                        "{what}"
                    );
                }
                let frame: &[u8] = &cut.bytes;
                let text = String::from_utf8_lossy(frame);
                match caller {
                    "bedrock" => {
                        assert_eq!(
                            frame,
                            wire::mid_stream_error_bytes(
                                HOST,
                                "bedrock",
                                true,
                                wire::MID_STREAM_GENERIC_DETAIL,
                                None
                            )
                            .as_slice(),
                            "{what}"
                        );
                        let mut buf = frame.to_vec();
                        let frames = busbar_plane_llm::codec::eventstream::drain_frames_checked(
                            &mut buf, None,
                        )
                        .0;
                        assert_eq!((frames.len(), buf.len()), (1, 0), "{what}");
                        assert!(text.contains("internalServerException"), "{what}");
                    }
                    "anthropic" => {
                        assert!(text.starts_with("event: error\n"), "{what}: {text}");
                        assert_eq!(text.matches("\n\n").count(), 1, "{what}: {text}");
                    }
                    "responses" => {
                        assert!(
                            text.starts_with("event: response.failed\n"),
                            "{what}: {text}"
                        );
                        assert_eq!(text.matches("\n\n").count(), 1, "{what}: {text}");
                    }
                    _ => {
                        assert!(
                            text.starts_with("data: ") && text.ends_with("\n\n"),
                            "{what}: {text}"
                        );
                        assert!(!text.contains("event:"), "{what}: {text}");
                        assert_eq!(text.matches("\n\n").count(), 1, "{what}: {text}");
                        let data: Value = serde_json::from_str(
                            text.trim_start_matches("data: ").trim_end_matches("\n\n"),
                        )
                        .expect("JSON");
                        if caller == "cohere" {
                            assert_eq!(data["type"], "message-end", "{what}: {data}");
                            assert!(
                                data["delta"]["finish_reason"]
                                    .as_str()
                                    .is_some_and(|reason| reason.starts_with("ERROR")),
                                "{what}: {data}"
                            );
                        } else {
                            assert!(data.get("error").is_some(), "{what}: {data}");
                        }
                    }
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 3 * 6);
    }

    const MARKER: &str = "hi";
    const IN_TOK: u64 = 11;
    const OUT_TOK: u64 = 7;

    /// Event-stream frames: each `(event, data)`, the event line left out when empty.
    fn sse(frames: &[(&str, Value)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (event, data) in frames {
            busbar_plane_llm::codec::dialect::write_sse_frame(&mut out, event, data);
        }
        out
    }

    /// The far end's answer, in `far`'s dialect, to a stream request: its content type and bytes,

    fn upstream_answer(far: &str, stream: bool) -> (&'static str, Vec<u8>) {
        let usage_chat = json!({"prompt_tokens": IN_TOK, "completion_tokens": OUT_TOK,
            "total_tokens": IN_TOK + OUT_TOK});
        match (far, stream) {
            ("anthropic", true) => (
                "text/event-stream",
                sse(&[
                    (
                        "message_start",
                        json!({"type": "message_start", "message": {"id": "msg_up",
                        "type": "message", "role": "assistant", "model": "m0", "content": [],
                        "stop_reason": null, "usage": {"input_tokens": IN_TOK, "output_tokens": 0}}}),
                    ),
                    (
                        "content_block_start",
                        json!({"type": "content_block_start", "index": 0,
                        "content_block": {"type": "text", "text": ""}}),
                    ),
                    (
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": MARKER}}),
                    ),
                    (
                        "content_block_stop",
                        json!({"type": "content_block_stop", "index": 0}),
                    ),
                    (
                        "message_delta",
                        json!({"type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"output_tokens": OUT_TOK}}),
                    ),
                    ("message_stop", json!({"type": "message_stop"})),
                ]),
            ),
            ("anthropic", false) => (
                "application/json",
                json!({"id": "msg_up", "type": "message", "role": "assistant", "model": "m0",
                    "content": [{"type": "text", "text": MARKER}], "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {"input_tokens": IN_TOK, "output_tokens": OUT_TOK}})
                .to_string()
                .into_bytes(),
            ),
            ("openai", true) => {
                let base = json!({"id": "chatcmpl-up", "object": "chat.completion.chunk",
                    "created": 0, "model": "m0"});
                let mut first = base.clone();
                first["choices"] = json!([{"index": 0,
                    "delta": {"role": "assistant", "content": MARKER}, "finish_reason": null}]);
                let mut last = base;
                last["choices"] = json!([{"index": 0, "delta": {}, "finish_reason": "stop"}]);
                last["usage"] = usage_chat;
                let mut body = sse(&[("", first), ("", last)]);
                body.extend_from_slice(busbar_plane_llm::codec::dialect::SSE_DONE_FRAME);
                ("text/event-stream", body)
            }
            ("openai", false) => (
                "application/json",
                json!({"id": "chatcmpl-up", "object": "chat.completion", "created": 0,
                    "model": "m0",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": MARKER},
                        "finish_reason": "stop"}],
                    "usage": usage_chat})
                .to_string()
                .into_bytes(),
            ),
            ("cohere", true) => {
                let mut body = sse(&[
                    (
                        "",
                        json!({"type": "message-start", "id": "cohere-up",
                        "delta": {"message": {"role": "assistant", "content": []}}}),
                    ),
                    (
                        "",
                        json!({"type": "content-delta", "index": 0,
                        "delta": {"message": {"content": {"type": "text", "text": MARKER}}}}),
                    ),
                    (
                        "",
                        json!({"type": "message-end", "delta": {"finish_reason": "COMPLETE",
                        "usage": {"tokens": {"input_tokens": IN_TOK, "output_tokens": OUT_TOK}}}}),
                    ),
                ]);
                body.extend_from_slice(busbar_plane_llm::codec::dialect::SSE_DONE_FRAME);
                ("text/event-stream", body)
            }
            ("cohere", false) => (
                "application/json",
                json!({"id": "cohere-up", "finish_reason": "COMPLETE",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": MARKER}]},
                    "usage": {"billed_units": {"input_tokens": IN_TOK, "output_tokens": OUT_TOK},
                        "tokens": {"input_tokens": IN_TOK, "output_tokens": OUT_TOK}}})
                .to_string()
                .into_bytes(),
            ),
            ("gemini", stream) => {
                let body = json!({"candidates": [{"content": {"role": "model",
                    "parts": [{"text": MARKER}]}, "finishReason": "STOP", "index": 0}],
                    "usageMetadata": {"promptTokenCount": IN_TOK, "candidatesTokenCount": OUT_TOK,
                        "totalTokenCount": IN_TOK + OUT_TOK},
                    "modelVersion": "m0"});
                if stream {
                    ("text/event-stream", sse(&[("", body)]))
                } else {
                    ("application/json", body.to_string().into_bytes())
                }
            }
            ("bedrock", true) => {
                let frames: [(&str, Value); 5] = [
                    ("messageStart", json!({"role": "assistant"})),
                    (
                        "contentBlockDelta",
                        json!({"delta": {"text": MARKER}, "contentBlockIndex": 0}),
                    ),
                    ("contentBlockStop", json!({"contentBlockIndex": 0})),
                    ("messageStop", json!({"stopReason": "end_turn"})),
                    (
                        "metadata",
                        json!({"usage": {"inputTokens": IN_TOK, "outputTokens": OUT_TOK,
                        "totalTokens": IN_TOK + OUT_TOK}, "metrics": {"latencyMs": 12}}),
                    ),
                ];
                let mut body = Vec::new();
                for (event, payload) in frames {
                    body.extend(busbar_plane_llm::codec::eventstream::encode_frame(
                        event,
                        &busbar_plane_llm::codec::json::to_vec(&payload).expect("frame JSON"),
                    ));
                }
                (EVENTSTREAM, body)
            }
            ("bedrock", false) => (
                "application/json",
                json!({"output": {"message": {"role": "assistant", "content": [{"text": MARKER}]}},
                    "stopReason": "end_turn",
                    "usage": {"inputTokens": IN_TOK, "outputTokens": OUT_TOK,
                        "totalTokens": IN_TOK + OUT_TOK},
                    "metrics": {"latencyMs": 12}})
                .to_string()
                .into_bytes(),
            ),
            ("responses", true) => {
                let item_done = json!({"type": "message", "id": "msg_up", "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": MARKER, "annotations": []}]});
                let usage = json!({"input_tokens": IN_TOK,
                    "input_tokens_details": {"cached_tokens": 0}, "output_tokens": OUT_TOK,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": IN_TOK + OUT_TOK});
                (
                    "text/event-stream",
                    sse(&[
                        (
                            "response.created",
                            json!({"type": "response.created",
                            "sequence_number": 0, "response": {"id": "resp_up", "object": "response",
                            "created_at": 0, "model": "m0", "status": "in_progress", "error": null,
                            "output": [], "usage": null}}),
                        ),
                        (
                            "response.output_item.added",
                            json!({"type": "response.output_item.added",
                            "sequence_number": 1, "output_index": 0, "item": {"type": "message",
                            "id": "msg_up", "role": "assistant", "status": "in_progress",
                            "content": []}}),
                        ),
                        (
                            "response.output_text.delta",
                            json!({"type": "response.output_text.delta",
                            "sequence_number": 2, "output_index": 0, "content_index": 0,
                            "item_id": "msg_up", "delta": MARKER}),
                        ),
                        (
                            "response.output_item.done",
                            json!({"type": "response.output_item.done",
                            "sequence_number": 3, "output_index": 0, "item": item_done}),
                        ),
                        (
                            "response.completed",
                            json!({"type": "response.completed",
                            "sequence_number": 4, "response": {"id": "resp_up", "object": "response",
                            "created_at": 0, "model": "m0", "status": "completed", "error": null,
                            "output": [item_done], "usage": usage}}),
                        ),
                    ]),
                )
            }
            ("responses", false) => (
                "application/json",
                json!({"id": "resp_up", "object": "response", "status": "completed", "model": "m0",
                    "output": [{"type": "message", "id": "msg_up", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": MARKER, "annotations": []}]}],
                    "usage": {"input_tokens": IN_TOK, "output_tokens": OUT_TOK,
                        "total_tokens": IN_TOK + OUT_TOK}})
                .to_string()
                .into_bytes(),
            ),
            (other, _) => panic!("no far-end fixture for {other}"),
        }
    }

    /// One `stream: true` responses request answered by a far end of `far` (a native stream when

    fn responses_case(far: &'static str, stream: bool) {
        let label = format!("responses<-{far} (stream: {stream})");
        let (content_type, body) = upstream_answer(far, stream);
        let (arrived, lane) = ask("responses", far, true);
        let ctx = reply_ctx(&arrived, &lane);
        let head: &[(&[u8], &[u8])] = &[(b"content-type", content_type.as_bytes())];
        let pieces: Vec<&[u8]> = body.chunks(97).collect();
        let read = answer(&ctx, 200, head, &pieces, false);
        assert_eq!(read.status, 200, "{label}");
        let read_type = read.value("content-type").unwrap_or_default();
        {
            assert!(
                read_type.starts_with("text/event-stream"),
                "{label}: {read_type}"
            );
            let text = read.text();
            assert!(!text.contains("[DONE]"), "{label}: {text}");
            let mut frames: Vec<(String, String)> = Vec::new();
            for frame in text.split("\n\n").filter(|frame| !frame.trim().is_empty()) {
                let mut event = String::new();
                let mut data: Vec<&str> = Vec::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event:") {
                        event = rest.trim().to_string();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data.push(rest.strip_prefix(' ').unwrap_or(rest));
                    }
                }
                frames.push((event, data.join("\n")));
            }
            assert!(!frames.is_empty(), "{label}");
            assert_eq!(frames[0].0, "response.created", "{label}: {text}");
            let (last_event, last_data) = frames.last().expect("a terminal frame");
            assert_eq!(last_event, "response.completed", "{label}: {text}");
            let completed: Value = serde_json::from_str(last_data).expect("JSON");
            assert_eq!(completed["type"], "response.completed", "{label}");
            assert!(
                completed["sequence_number"].is_u64(),
                "{label}: {last_data}"
            );
            let response = &completed["response"];
            assert_eq!(response["object"], "response", "{label}");
            assert_eq!(response["status"], "completed", "{label}");
            assert!(
                response["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("resp_")),
                "{label}: {last_data}"
            );
            assert_eq!(
                response["output"][0]["content"][0]["text"], MARKER,
                "{label}: {last_data}"
            );
            assert_eq!(response["usage"]["input_tokens"], IN_TOK, "{label}");
            assert_eq!(response["usage"]["output_tokens"], OUT_TOK, "{label}");
            assert_eq!(
                response["usage"]["total_tokens"],
                IN_TOK + OUT_TOK,
                "{label}"
            );
            for (event, data) in &frames {
                let payload: Value = serde_json::from_str(data).expect("JSON");
                assert_eq!(payload["type"], event.as_str(), "{label}");
            }
        }
        assert_eq!(read.tokens(), (IN_TOK, OUT_TOK), "{label}");
        assert_eq!(read.settlements(), 1, "{label}: billed once");
        assert!(
            read.pieces
                .iter()
                .all(|piece| piece.units.tokens_in <= IN_TOK && piece.units.tokens_out <= OUT_TOK),
            "{label}: never more than the far end reported"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_anthropic_stream`

    #[test]
    fn responses_stream_over_anthropic_stream() {
        responses_case("anthropic", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_anthropic_buffered`

    #[test]
    fn responses_stream_over_anthropic_buffered() {
        responses_case("anthropic", false);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_openai_stream`

    #[test]
    fn responses_stream_over_openai_stream() {
        responses_case("openai", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_openai_buffered`

    #[test]
    fn responses_stream_over_openai_buffered() {
        responses_case("openai", false);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_cohere_stream`

    #[test]
    fn responses_stream_over_cohere_stream() {
        responses_case("cohere", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_cohere_buffered`

    #[test]
    fn responses_stream_over_cohere_buffered() {
        responses_case("cohere", false);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_gemini_stream`

    #[test]
    fn responses_stream_over_gemini_stream() {
        responses_case("gemini", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_gemini_buffered`

    #[test]
    fn responses_stream_over_gemini_buffered() {
        responses_case("gemini", false);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_bedrock_stream`

    #[test]
    fn responses_stream_over_bedrock_stream() {
        responses_case("bedrock", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_bedrock_buffered`

    #[test]
    fn responses_stream_over_bedrock_buffered() {
        responses_case("bedrock", false);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_responses_stream`

    #[test]
    fn responses_stream_over_responses_stream() {
        responses_case("responses", true);
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/responses_ingress_stream_tests.rs::responses_stream_over_responses_buffered`

    #[test]
    fn responses_stream_over_responses_buffered() {
        responses_case("responses", false);
    }

    fn message_with_usage(usage: &str) -> String {
        format!(
            r#"{{"id":"msg_1","type":"message","role":"assistant","model":"claude-x","content":[{{"type":"text","text":"hi"}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{usage}}}"#
        )
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/unreadable_usage_floor_tests.rs::buffered_relay_refused_usage_bills_the_floor_not_zero`

    #[test]
    fn buffered_relay_refused_usage_bills_only_the_readable_counts() {
        let (arrived, lane) = ask("anthropic", "anthropic", false);
        let ctx = reply_ctx(&arrived, &lane);
        let readable = message_with_usage(r#"{"input_tokens":1500,"output_tokens":9}"#);
        let read = answer(&ctx, 200, JSON_HEAD, &[readable.as_bytes()], false);
        assert_eq!(read.body, readable.as_bytes());
        assert_eq!(read.tokens(), (1500, 9));
        assert_eq!(read.tokens().0 + read.tokens().1, 1509);

        let refused = message_with_usage(r#"{"input_tokens":"1500","output_tokens":9}"#);
        let read = answer(&ctx, 200, JSON_HEAD, &[refused.as_bytes()], false);
        assert_eq!(read.body, refused.as_bytes(), "the body relays verbatim");
        assert_eq!(
            read.tokens(),
            (0, 9),
            "v1.5.5's figure: the readable count, nothing else"
        );
    }

    /// REHOMES `crates/busbar-llm/src/engine/tests/unreadable_usage_floor_tests.rs::stream_refused_usage_bills_the_floor_not_zero`

    #[test]
    fn stream_refused_usage_bills_only_the_readable_counts() {
        let (arrived, lane) = ask("openai", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let base = json!({"id": "chatcmpl-refused", "object": "chat.completion.chunk",
            "created": 0, "model": "m0"});
        let mut first = base.clone();
        first["choices"] = json!([{"index": 0,
            "delta": {"role": "assistant", "content": "hello"}, "finish_reason": null}]);
        let mut last = base.clone();
        last["choices"] = json!([{"index": 0, "delta": {}, "finish_reason": "stop"}]);
        let mut usage = base;
        usage["choices"] = json!([]);
        usage["usage"] = json!({"prompt_tokens": "1500", "completion_tokens": 9});
        let mut far = data_frames(&[&first.to_string(), &last.to_string(), &usage.to_string()]);
        far.extend_from_slice(busbar_plane_llm::codec::dialect::SSE_DONE_FRAME);
        let read = answer(&ctx, 200, SSE_HEAD, &[&far], false);
        assert_eq!(read.status, 200);
        assert_eq!(
            read.tokens(),
            (0, 9),
            "v1.5.5's figure: the readable count, nothing else"
        );
    }

    #[test]
    fn local_entropy_failure_keeps_actual_reply_units_and_late_cancel_accounting() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct LimitedEntropy(AtomicUsize);
        impl busbar_contract::codec::CodecHost for LimitedEntropy {
            fn fill_entropy(&self, out: &mut [u8]) -> bool {
                out.fill(7);
                self.0
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_ok()
            }
            fn wall_clock_now(&self) -> Option<u64> {
                Some(1)
            }
        }
        let opening = b"data: {\"id\":\"upstream-id\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"unwritten-canary\"},\"finish_reason\":null}]}\n\n";
        let finished = b"data: {\"id\":\"upstream-id\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n";
        let failure =
            b"data: {\"error\":{\"type\":\"server_error\",\"code\":\"upstream-failure\"}}\n\n";
        let (arrived, lane) = ask("responses", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);

        for draws in [0, 1] {
            for upstream_failed in [false, true] {
                let host = LimitedEntropy(AtomicUsize::new(draws));
                let mut reply = Reply::new(&host, &ctx, 200, SSE_HEAD);
                let opening = reply.feed(&host, &ctx, opening, false, AT);
                let text = String::from_utf8_lossy(&opening.bytes);
                assert!(text.contains("error"), "{text}");
                assert!(!text.contains("unwritten-canary"), "{text}");
                let _ = reply.feed(&host, &ctx, finished, false, AT);
                let tail = if upstream_failed {
                    failure.as_slice()
                } else {
                    b"".as_slice()
                };
                let end = reply.feed(&host, &ctx, tail, true, AT);
                assert!(end.done);
                assert_eq!(end.verdict, Verdict::Hard);
                if upstream_failed {
                    assert_eq!(end.fault, Some(Fault::Transient("stream-terminal-error")));
                    assert_eq!((end.units.tokens_in, end.units.tokens_out), (0, 0));
                    assert!(
                        !reply.partial(),
                        "an upstream terminal error retains its refund policy"
                    );
                } else {
                    assert_eq!(end.fault, Some(Fault::Transient("stream-translate-abort")));
                    assert_eq!((end.units.tokens_in, end.units.tokens_out), (7, 3));
                    assert!(
                        reply.partial(),
                        "a late cancellation must retain the executed units too"
                    );
                }
            }
        }
    }

    #[test]
    fn entropy_failure_only_in_final_flush_keeps_actual_reply_units() {
        struct Silent;
        impl busbar_contract::codec::CodecHost for Silent {
            fn fill_entropy(&self, _out: &mut [u8]) -> bool {
                false
            }
            fn wall_clock_now(&self) -> Option<u64> {
                Some(1)
            }
        }
        let (arrived, lane) = ask("responses", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane);
        let mut reply = Reply::new(&Silent, &ctx, 200, SSE_HEAD);
        let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n";
        let first = reply.feed(&Silent, &ctx, delta, false, AT);
        assert!(first.bytes.is_empty(), "the terminal delta is deferred");
        let end = reply.feed(&Silent, &ctx, b"", true, AT);
        assert_eq!(end.verdict, Verdict::Hard);
        assert_eq!(end.fault, Some(Fault::Transient("stream-translate-abort")));
        assert_eq!((end.units.tokens_in, end.units.tokens_out), (0, 3));
        assert!(reply.partial());
        let body = String::from_utf8_lossy(&end.bytes);
        assert!(body.contains("error"), "{body}");
        assert!(!body.contains("response.completed"), "{body}");

        let protocol = busbar_plane_llm::codec::proto_codec::protocol_for("responses").unwrap();
        let original = busbar_contract::protocol::IrError {
            class: StatusClass::ServerError,
            provider_signal: Some("original-provider-failure".into()),
            retry_after: None,
        };
        let (_, framed) = protocol
            .writer()
            .write_error_frame(&Silent, &original)
            .unwrap();
        let text = framed.to_string();
        assert!(text.contains("original-provider-failure"), "{text}");
        assert!(!text.contains("0000000000"), "{text}");
    }

    /// One answer read to its end in one far-end piece: the reply (for its cancel reading) and

    fn ended(ctx: &ReplyCtx<'_>, head: &[(&[u8], &[u8])], far: &[u8]) -> (Reply, Piece<'static>) {
        let mut reply = Reply::new(HOST, ctx, 200, head);
        let end = owned(reply.feed(HOST, ctx, far, true, AT));
        assert!(end.done, "{end:?}");
        (reply, end)
    }

    /// A RELAYED END THAT BILLS ITS UNITS BILLS THEM WHEN THE CALLER LEAVES DURING ITS LAST WINDOW

    #[test]
    fn a_relayed_end_that_bills_bills_when_the_caller_leaves_during_its_last_window() {
        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let (reply, end) = ended(&ctx, JSON_HEAD, &cohere_body("ERROR"));
        assert_eq!(
            end.fault,
            Some(Fault::Transient("upstream-generation-failed"))
        );
        assert_eq!((end.units.tokens_in, end.units.tokens_out), (10, 5));
        assert!(
            reply.partial(),
            "a same-dialect failed generation bills its 10 / 5 on a cancel in its last window"
        );

        let (arrived, lane) = ask("openai", "gemini", true);
        let ctx = reply_ctx(&arrived, &lane);
        let (reply, end) = ended(&ctx, SSE_HEAD, &gemini_stream("MALFORMED_FUNCTION_CALL"));
        assert_eq!(end.verdict, Verdict::Hard);
        assert_eq!((end.units.tokens_in, end.units.tokens_out), (10, 5));
        assert!(
            reply.partial(),
            "a streamed failed generation bills its 10 / 5 on a cancel in its last window"
        );

        let (arrived, lane) = ask("anthropic", "openai", true);
        let ctx = reply_ctx(&arrived, &lane);
        let mut reply = Reply::new(HOST, &ctx, 200, SSE_HEAD);
        let _ = reply.feed(HOST, &ctx, USAGE_CHUNK, false, AT);
        let overflow = overflow();
        let end = reply.feed(HOST, &ctx, &overflow, true, AT);
        assert_eq!(end.fault, Some(Fault::Transient("stream-translate-abort")));
        assert_eq!((end.units.tokens_in, end.units.tokens_out), (600, 400));
        assert!(
            reply.partial(),
            "an aborted translation bills what streamed on a cancel in its last window (#62)"
        );

        let (arrived, lane) = ask("openai", "gemini", true);
        let ctx = reply_ctx(&arrived, &lane);
        let (reply, end) = ended(&ctx, SSE_HEAD, &gemini_stream("STOP"));
        assert_eq!(end.fault, None);
        assert!(
            reply.partial(),
            "a clean streamed end bills on a late cancel"
        );

        let mut arrived = arrived_for(
            "responses",
            OpVerb::CHAT,
            Some(json!({"model": "p", "stream": true, "input": "hi"})),
            false,
        );
        arrived.path = "/v1/responses".to_string();
        let responses = super::lane("responses");
        let ctx = reply_ctx(&arrived, &responses);
        let failed = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"in_progress\",\"model\":\"m-1\",\"output\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hel\"}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1,\"status\":\"failed\",\"model\":\"m-1\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"boom\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n",
        );
        let (reply, end) = ended(&ctx, SSE_HEAD, failed.as_bytes());
        assert_eq!((end.units.tokens_in, end.units.tokens_out), (0, 0));
        assert!(
            !reply.partial(),
            "a stream the far end failed bills nothing on a late cancel"
        );
    }

    /// ZERO HOST CALLS PER CHUNK (`on_piece`). The door reads the host's

    #[test]
    fn only_the_last_piece_of_a_whole_answer_reads_the_time() {
        let (arrived, lane) = ask("openai", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let reply = Reply::new(HOST, &ctx, 200, JSON_HEAD);
        assert!(
            !reply.takes_time(false),
            "an earlier piece of a whole answer"
        );
        assert!(reply.takes_time(true), "the whole answer's last piece");

        let (arrived, lane) = ask("openai", "anthropic", true);
        let ctx = reply_ctx(&arrived, &lane);
        let reply = Reply::new(HOST, &ctx, 200, SSE_HEAD);
        assert!(
            !reply.takes_time(false) && !reply.takes_time(true),
            "a relayed stream"
        );

        let (arrived, lane) = ask("cohere", "cohere", false);
        let ctx = reply_ctx(&arrived, &lane);
        let reply = Reply::new(HOST, &ctx, 200, JSON_HEAD);
        assert!(!reply.takes_time(true), "a same-dialect body relays");

        let mut reply = Reply::new(HOST, &ctx, 500, JSON_HEAD);
        assert!(!reply.takes_time(true), "a judged failure");
        let _ = reply.feed(HOST, &ctx, b"{}", true, AT);
        assert!(!reply.takes_time(true), "a finished answer");
    }
}

/// A TRANSLATED STREAM IS TIMED ON THE HOST'S ONE CLOCK. A Bedrock caller's

#[test]
fn a_translated_stream_is_timed_on_the_hosts_clock() {
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Clocked(AtomicU64);
    impl busbar_contract::codec::CodecHost for Clocked {
        fn fill_entropy(&self, out: &mut [u8]) -> bool {
            out.fill(7);
            true
        }
        fn wall_clock_now(&self) -> Option<u64> {
            Some(1_700_000_000)
        }
        fn monotonic_ns(&self) -> Option<u64> {
            Some(self.0.load(Ordering::Relaxed))
        }
    }
    let host = Clocked(AtomicU64::new(5_000_000_000));
    let far = native_stream("anthropic");
    let (first, rest) = far.split_at(far.len() / 2);
    let mut r = Relay::new(relay_ctx("bedrock", "anthropic", true));
    let mut out = Vec::new();
    if let (Fed::Bytes(b), _) = r.feed(&host, first) {
        out.extend_from_slice(&b);
    }
    host.0.fetch_add(250_000_000, Ordering::Relaxed);
    if let (Fed::Bytes(b), _) = r.feed(&host, rest) {
        out.extend_from_slice(&b);
    }
    out.extend(r.end(&host).bytes);
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("\"latencyMs\":250"),
        "the metadata frame reports the host's 250 ms: {text}"
    );
}

/// AN OPENAI FAR END'S WHOLE ANSWER READS AS AN ANTHROPIC MESSAGE: an anthropic caller served by an

#[test]
fn an_openai_whole_answer_reads_as_an_anthropic_message_naming_the_serving_model() {
    let far = br#"{"object":"chat.completion","model":"glm-4.5","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#;
    let w = whole::translate(HOST, &ctx("anthropic", "openai", false, false), 200, far);
    assert_eq!(w.end, WholeEnd::Delivered, "{:?}", w.refused);
    assert_eq!(w.answer.status, 200);
    let got: Value = serde_json::from_slice(&w.answer.body).expect("anthropic JSON");
    assert_eq!(got["type"], "message", "{got}");
    assert_eq!(got["content"][0]["type"], "text", "{got}");
    assert_eq!(got["content"][0]["text"], "hi", "{got}");
    assert_eq!(got["stop_reason"], "end_turn", "{got}");
    assert_eq!(got["model"], "glm-4.5", "the serving model survives: {got}");
    assert!(got.get("choices").is_none(), "no raw openai body: {got}");
}

#[test]
fn malformed_anthropic_answer_keeps_reported_usage_at_the_reply_boundary() {
    let far = br#"{"role":"assistant","content":"malformed","usage":{"input_tokens":17,"output_tokens":9}}"#;
    let w = whole::translate(HOST, &ctx("openai", "anthropic", false, false), 200, far);
    assert_eq!(w.end, WholeEnd::NotTranslatable);
    assert_eq!(w.answer.status, 500);
    assert!(
        w.refused.is_some(),
        "the original decode refusal remains explained"
    );
    let token = busbar_plane_llm::exchange::reply::wire::token_usage_of(&w.usage).unwrap();
    assert_eq!((token.input, token.output), (17, 9));
    let tapped = chat_handler("anthropic")
        .extract_usage(HOST, "anthropic", far)
        .unwrap()
        .unwrap();
    assert_eq!((tapped.input, tapped.output), (17, 9));
    let nested = br#"{"role":"assistant","content":"malformed","metadata":{"usage":{"input_tokens":999,"output_tokens":888}}}"#;
    let no_report = whole::translate(HOST, &ctx("openai", "anthropic", false, false), 200, nested);
    assert_eq!(no_report.end, WholeEnd::NotTranslatable);
    assert_eq!(
        no_report.usage, None,
        "nested metadata is not provider usage"
    );
    assert!(chat_handler("anthropic")
        .extract_usage(HOST, "anthropic", nested)
        .is_err());
    let decoy = br#"{"role":"assistant","content":"malformed","usage":{"input_tokens":17,"output_tokens":9},"metadata":{"usage":{"input_tokens":999,"output_tokens":888}}}"#;
    let reported = whole::translate(HOST, &ctx("openai", "anthropic", false, false), 200, decoy);
    let token = busbar_plane_llm::exchange::reply::wire::token_usage_of(&reported.usage).unwrap();
    assert_eq!((token.input, token.output), (17, 9));
    let tapped = chat_handler("anthropic")
        .extract_usage(HOST, "anthropic", decoy)
        .unwrap()
        .unwrap();
    assert_eq!((tapped.input, tapped.output), (17, 9));

    let arrived = arrival("openai", false);
    let far_lane = lane("anthropic");
    let reply_ctx = ReplyCtx {
        arrived: &arrived,
        lane: &far_lane,
        intent: stream_intent(chat_handler("openai"), arrived.parsed.as_ref()),
        passthrough: false,
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    };
    let mut reply = Reply::new(
        HOST,
        &reply_ctx,
        200,
        &[(b"content-type", b"application/json")],
    );
    let piece = reply.feed(HOST, &reply_ctx, far, true, AT);
    assert_eq!(piece.head.unwrap().status, 500);
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (17, 9));
    assert!(
        reply.partial(),
        "reported work survives a failed output and a later leave"
    );
}

#[test]
fn empty_speech_output_is_a_failed_delivery_with_original_flat_billing() {
    let mut context = ctx("openai", "openai", false, false);
    context.operation = OpVerb::SPEECH;
    let failed = whole::translate(HOST, &context, 200, b"");
    assert_eq!(failed.end, WholeEnd::NotTranslatable);
    assert_eq!(failed.answer.status, 500);
    assert_eq!(failed.usage, Some(busbar_contract::billing::Billing::Flat));
    let success = whole::translate(HOST, &context, 200, b"RIFF-not-empty");
    assert_eq!(success.end, WholeEnd::Delivered);
    assert_eq!(success.usage, failed.usage);
    assert_eq!(success.answer.body, b"RIFF-not-empty");
}

#[test]
fn malformed_rerank_retains_full_billing_variants_and_units() {
    use busbar_contract::billing::Billing;
    use busbar_plane_llm::exchange::reply::{wire, Units};
    for value in [
        Some(serde_json::json!(7)),
        Some(serde_json::json!(0)),
        Some(serde_json::json!(u64::MAX)),
        None,
        Some(serde_json::json!("7")),
    ] {
        let mut far = serde_json::json!({"results":[{"index":0,"relevance_score":"malformed"}]});
        if let Some(value) = &value {
            far["meta"] = serde_json::json!({"billed_units":{"search_units":value}});
        }
        let expected = match value.as_ref().and_then(Value::as_u64) {
            Some(count) => Billing::Counted {
                class: "search_units".into(),
                count,
            },
            None => Billing::Flat,
        };
        let mut context = ctx("bedrock", "cohere", false, false);
        context.operation = OpVerb::RERANK;
        let failed = whole::translate(HOST, &context, 200, &serde_json::to_vec(&far).unwrap());
        assert_eq!(failed.end, WholeEnd::NotTranslatable);
        assert_eq!(failed.answer.status, 500);
        assert!(failed.refused.is_some());
        assert_eq!(failed.usage, Some(expected.clone()));
        let units = Units::of(
            wire::token_usage_of(&failed.usage).as_ref(),
            wire::open_units_of(&failed.usage),
        );
        assert_eq!((units.tokens_in, units.tokens_out), (0, 0));
        assert_eq!(
            units.open.get("search_units").copied(),
            value.as_ref().and_then(Value::as_u64)
        );
        far["results"][0]["relevance_score"] = serde_json::json!(0.75);
        let success = whole::translate(HOST, &context, 200, &serde_json::to_vec(&far).unwrap());
        assert_eq!(success.end, WholeEnd::Delivered);
        assert_eq!(success.usage, Some(expected));
    }
}

/// A banner and the `error_map_value` it carried, per event.
type Banners = Vec<(String, Option<String>)>;

/// Every `diag` banner, and every uncoded line carrying an `error_map_value` (by its message), with

fn banners_of(f: impl FnOnce()) -> Banners {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};

    #[derive(Default)]
    struct Fields {
        diag: Option<String>,
        message: Option<String>,
        value: Option<String>,
    }
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "diag" => self.diag = Some(format!("{value:?}")),
                "message" => self.message = Some(format!("{value:?}")),
                "error_map_value" => self.value = Some(format!("{value:?}")),
                _ => {}
            }
        }
    }
    struct Capture(Arc<Mutex<Banners>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = Fields::default();
            event.record(&mut fields);

            let line = fields
                .diag
                .or_else(|| fields.value.as_ref().and(fields.message));
            if let Some(line) = line {
                self.0.lock().unwrap().push((line, fields.value));
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::with_default(Capture(seen.clone()), f);
    let mut seen = seen.lock().unwrap();
    std::mem::take(&mut *seen)
}

/// A provider `error_map` entry that names no status class is ignored, and the operator is told so

#[test]
fn an_error_map_class_that_names_no_class_is_reported_once() {
    let em = map(&[
        ("1302", "rate_limt_by_code"),
        ("overloaded_error", "overloaded_by_type"),
        ("quota", "billing"),
    ]);

    let code = "error_map maps an error to an unrecognized status class; the mapping is IGNORED and \
                classification falls through to HTTP status. Valid classes: rate_limit, overloaded, \
                server_error, timeout, network, auth, billing, client_error, context_length"
        .to_string();
    let seen = banners_of(|| {
        let sig = normalize(400, &raw(400, Some("1302"), None), &em);
        assert_eq!(sig.class, StatusClass::ClientError);

        let _ = normalize(400, &raw(400, Some("1302"), None), &em);
        let _ = normalize(500, &raw(500, None, Some("overloaded_error")), &em);

        let _ = normalize(402, &raw(402, Some("quota"), None), &em);
    });
    let want: Banners = vec![
        (code.clone(), Some("\"rate_limt_by_code\"".to_string())),
        (code, Some("\"overloaded_by_type\"".to_string())),
    ];
    assert_eq!(seen, want);
}

#[test]
fn failed_responses_usage_is_observed_while_same_and_cross_dialect_refunds_stay_zero() {
    let far = br#"{"id":"resp_failed","status":"failed","output":[],"error":{"code":"server_error","message":"failed turn"},"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}"#;
    let observed = chat_handler("responses")
        .extract_usage(HOST, "responses", far)
        .unwrap()
        .unwrap();
    assert_eq!(
        (observed.input, observed.output),
        (5, 2),
        "reported usage remains observable independently of refund policy"
    );
    for ingress in ["responses", "openai", "anthropic"] {
        let mut arrived = arrival(ingress, false);
        if ingress == "responses" {
            arrived.path = "/v1/responses".into();
            arrived.parsed = Some(serde_json::json!({"model":"p","input":"hi"}));
            arrived.body = serde_json::to_vec(arrived.parsed.as_ref().unwrap()).unwrap();
        }
        let lane = lane("responses");
        let reply_ctx = ReplyCtx {
            arrived: &arrived,
            lane: &lane,
            intent: stream_intent(chat_handler(ingress), arrived.parsed.as_ref()),
            passthrough: false,
            translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
        };
        let mut reply = Reply::new(HOST, &reply_ctx, 200, JSON_HEAD);
        let end = reply.feed(HOST, &reply_ctx, far, true, AT);
        assert!(end.done);
        assert_eq!(
            (end.units.tokens_in, end.units.tokens_out),
            (0, 0),
            "{ingress}: observing usage must not change the existing failure refund"
        );
        if ingress == "responses" {
            assert_eq!(
                end.bytes.as_ref(),
                far,
                "same-dialect failure bytes remain verbatim"
            );
        }
    }

    let cross = whole::translate(HOST, &ctx("openai", "responses", false, false), 200, far);
    assert_eq!(cross.end, WholeEnd::NotTranslatable);
    assert!(cross.refused.is_some());
    assert_eq!(cross.usage, None);
}

#[test]
fn responses_content_parse_failure_recovery_still_keeps_reported_work() {
    let far = br#"{"status":"completed","output":"unreadable content","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}"#;
    let observed = chat_handler("responses")
        .extract_usage(HOST, "responses", far)
        .unwrap()
        .unwrap();
    assert_eq!((observed.input, observed.output), (5, 2));
    let cross = whole::translate(HOST, &ctx("openai", "responses", false, false), 200, far);
    assert_eq!(cross.end, WholeEnd::NotTranslatable);
    assert!(
        cross.refused.is_some(),
        "the parse refusal must remain visible"
    );
    let kept = busbar_plane_llm::exchange::reply::wire::token_usage_of(&cross.usage).unwrap();
    assert_eq!(
        (kept.input, kept.output),
        (5, 2),
        "a content parse failure is distinct from an upstream-declared refunded failure"
    );
    let nested = br#"{"status":"completed","output":"unreadable content","metadata":{"usage":{"input_tokens":999,"output_tokens":888}}}"#;
    let no_report = whole::translate(HOST, &ctx("openai", "responses", false, false), 200, nested);
    assert_eq!(no_report.end, WholeEnd::NotTranslatable);
    assert_eq!(
        no_report.usage, None,
        "recovery uses only structurally located provider usage"
    );
}
