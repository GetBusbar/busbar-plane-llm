//! Bedrock wire identity and projection through the production error/whole reply seams.
use busbar_contract::{
    billing::Billing,
    codec::CodecHost,
    operation::OpVerb,
    protocol::HeadFields,
    upstream::{Disposition, StatusClass},
};
use busbar_plane_llm::exchange::reply::{
    failure::{judge, FarError},
    whole::{self, WholeCtx, WholeEnd},
};
use busbar_test_kit::warn_capture::WarnCapture;
use serde_json::{json, Value};
use std::collections::HashMap;
struct Host;
impl CodecHost for Host {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}
fn mapped(head: HeadFields<'_>, body: &[u8], want: StatusClass, signal: Option<&str>) {
    let map = HashMap::from([
        ("ThrottlingException".into(), "rate_limit".into()),
        ("OtherFault".into(), "billing".into()),
        ("A#B".into(), "timeout".into()),
    ]);
    let result = judge(
        &Host,
        "bedrock",
        "bedrock",
        OpVerb::CHAT,
        &map,
        false,
        &FarError {
            status: 422,
            head,
            body,
        },
    );
    let actual = result.signal.unwrap();
    assert_eq!(actual.class, want, "head={head:?} body={body:?}");
    assert_eq!(actual.provider_signal.as_deref(), signal);
    assert_eq!(result.disposition, want.disposition());
    assert_eq!(result.answer.status, 422);
    assert_eq!(result.answer.body, body);
    let expected: Vec<_> = head
        .iter()
        .map(|(n, v)| {
            (
                String::from_utf8(n.to_ascii_lowercase()).unwrap(),
                v.to_vec(),
            )
        })
        .collect();
    assert_eq!(
        result.answer.fields, expected,
        "native headers retain order and values"
    );
}
#[test]
fn header_identity_reaches_real_operator_map_without_json() {
    for body in [b"".as_slice(), b"not JSON"] {
        for identity in [
            "ThrottlingException",
            "ns#ThrottlingException",
            "ThrottlingException:http://coral.invalid/",
            " ns#ThrottlingException:http://coral.invalid/ ",
        ] {
            mapped(
                &[(b"X-aMzN-ErRoRtYpE", identity.as_bytes())],
                body,
                StatusClass::RateLimit,
                Some("ThrottlingException"),
            );
        }
    }
}
#[test]
fn body_code_and_type_normalize_before_real_operator_mapping() {
    for key in ["code", "__type"] {
        for identity in [
            "OtherFault",
            "ns#OtherFault",
            "OtherFault:http://coral.invalid/",
            "ns#OtherFault:http://coral.invalid/",
            " OtherFault ",
        ] {
            mapped(
                &[],
                &serde_json::to_vec(&json!({key:identity})).unwrap(),
                StatusClass::Billing,
                Some("OtherFault"),
            );
        }
    }
    mapped(
        &[],
        br#"{"__type":"ns#A#B:http://suffix/"}"#,
        StatusClass::Timeout,
        Some("A#B"),
    );
}
#[test]
fn carrier_precedence_skips_only_unusable_candidates() {
    let body = br#"{"code":"OtherFault","__type":"ThrottlingException"}"#;
    mapped(
        &[(b"x-amzn-errortype", b"ThrottlingException")],
        body,
        StatusClass::RateLimit,
        Some("ThrottlingException"),
    );
    mapped(
        &[(b"x-amzn-errortype", b"Unknown")],
        body,
        StatusClass::ClientError,
        None,
    );
    mapped(
        &[(b"x-amzn-errortype", b"throttlingexception")],
        body,
        StatusClass::ClientError,
        None,
    );
    mapped(&[], body, StatusClass::Billing, Some("OtherFault"));
    for invalid in [b"\xff".as_slice(), b" ", b"ns#:suffix", b":suffix"] {
        mapped(
            &[(b"x-amzn-errortype", invalid)],
            body,
            StatusClass::Billing,
            Some("OtherFault"),
        );
        mapped(
            &[
                (b"x-amzn-errortype", invalid),
                (b"X-Amzn-Errortype", b"ThrottlingException"),
                (b"x-amzn-errortype", b"OtherFault"),
            ],
            body,
            StatusClass::RateLimit,
            Some("ThrottlingException"),
        );
    }
    mapped(
        &[],
        br#"{"code":12,"__type":"OtherFault"}"#,
        StatusClass::Billing,
        Some("OtherFault"),
    );
    mapped(
        &[],
        br#"{"code":"ns#:suffix","__type":"OtherFault"}"#,
        StatusClass::Billing,
        Some("OtherFault"),
    );
    for body in [b"{}".as_slice(), b"bad", br#"{"code":[],"__type":false}"#] {
        mapped(&[], body, StatusClass::ClientError, None);
    }
}
#[test]
fn native_auth_context_and_supported_operation_controls() {
    let heads: HeadFields<'_> = &[
        (
            b"x-amzn-errortype",
            b"ns#ThrottlingException:http://coral.invalid/",
        ),
        (b"x-native", b"a\xffb"),
    ];
    let body = br#"{"message":"native detail"}"#;
    let map = HashMap::from([
        ("ThrottlingException".into(), "rate_limit".into()),
        ("native detail".into(), "timeout".into()),
    ]);
    for status in [401, 403] {
        let result = judge(
            &Host,
            "bedrock",
            "bedrock",
            OpVerb::CHAT,
            &map,
            true,
            &FarError {
                status,
                head: heads,
                body,
            },
        );
        assert!(result.signal.is_none());
        assert_eq!(result.disposition, Disposition::ClientFault);
        assert_eq!(result.answer.status, status);
        assert_eq!(result.answer.body, body);
        assert_eq!(result.answer.fields[1].1, b"a\xffb");
    }
    // Existing provider-message priority is unchanged even when a header identity maps.
    let result = judge(
        &Host,
        "bedrock",
        "bedrock",
        OpVerb::CHAT,
        &map,
        false,
        &FarError {
            status: 422,
            head: heads,
            body,
        },
    );
    let signal = result.signal.unwrap();
    assert_eq!(signal.class, StatusClass::Timeout);
    assert_eq!(signal.provider_signal.as_deref(), Some("native detail"));
    for (dialect, op) in [("openai", OpVerb::CHAT), ("bedrock", OpVerb::SPEECH)] {
        let result = judge(
            &Host,
            dialect,
            dialect,
            op,
            &map,
            false,
            &FarError {
                status: 422,
                head: heads,
                body: b"{}",
            },
        );
        assert_eq!(result.signal.unwrap().class, StatusClass::ClientError);
    }
    for (status, want) in [
        (400, StatusClass::ContextLength),
        (500, StatusClass::ServerError),
    ] {
        let body=br#"{"message":"Input is longer than the maximum number of tokens allowed","__type":"ValidationException"}"#;
        let result = judge(
            &Host,
            "bedrock",
            "bedrock",
            OpVerb::CHAT,
            &HashMap::new(),
            false,
            &FarError {
                status,
                head: &[],
                body,
            },
        );
        assert_eq!(result.signal.unwrap().class, want);
        assert_eq!(result.answer.body, body);
    }
}
fn translate(ingress: &str, egress: &str, op: OpVerb, body: &[u8]) -> (whole::Whole, WarnCapture) {
    let ctx = WholeCtx {
        ingress,
        egress,
        operation: op,
        model: "model",
        wants_stream: false,
        json_array: false,
        request: None,
        now_s: 1,
        elapsed_ms: None,
        translate_cap: 65536,
    };
    let cap = WarnCapture::default();
    let result =
        tracing::subscriber::with_default(cap.clone(), || whole::translate(&Host, &ctx, 200, body));
    assert_eq!(result.end, WholeEnd::Delivered, "{:?}", result.refused);
    (result, cap)
}
#[test]
fn lost_url_images_warn_once_audit_and_preserve_paid_work() {
    let secret = "https://private.invalid/SECRET-IMAGE?token=DO-NOT-LOG";
    for (data, want, count, drop) in [
        (json!([{"url":secret}]), json!({"images":[]}), 1, true),
        (
            json!([{"url":secret},{"url":secret}]),
            json!({"images":[]}),
            2,
            true,
        ),
        (
            json!([{"b64_json":"QQ=="},{"url":secret},{"b64_json":"Qg=="}]),
            json!({"images":["QQ==","Qg=="]}),
            3,
            true,
        ),
        (
            json!([{"b64_json":"QQ==","url":secret}]),
            json!({"images":["QQ=="]}),
            1,
            false,
        ),
        (
            json!([{"b64_json":"QQ=="},{"b64_json":"Qg=="}]),
            json!({"images":["QQ==","Qg=="]}),
            2,
            false,
        ),
        (json!([]), json!({"images":[]}), 0, false),
    ] {
        let body = serde_json::to_vec(&json!({"data":data})).unwrap();
        let (whole, cap) = translate("bedrock", "openai", OpVerb::IMAGE, &body);
        assert_eq!(whole.answer.body, serde_json::to_vec(&want).unwrap());
        assert!(matches!(whole.usage,Some(Billing::Images {count:n,..}) if n==count));
        assert_eq!(
            whole.dropped,
            if drop {
                vec!["ir.images[].url"]
            } else {
                vec![]
            }
        );
        assert_eq!(cap.count("path=ir.images[].url"), usize::from(drop));
        assert!(!cap.contains(secret));
        assert!(!cap.contains("SECRET-IMAGE"));
    }
}
fn bedrock_citation() -> Value {
    json!({"title":"Policy","source":"opaque:source#7","sourceContent":[{"text":"Quoted passage"}],"location":{"searchResultLocation":{"searchResultIndex":7,"start":3,"end":9}}})
}
fn anthropic_citation() -> Value {
    json!({"type":"search_result_location","title":"Policy","source":"opaque:source#7","cited_text":"Quoted passage","search_result_index":7,"start_block_index":3,"end_block_index":9})
}
fn bedrock_response(citation: Value) -> Value {
    json!({"output":{"message":{"role":"assistant","content":[{"citationsContent":{"content":[{"text":"Answer beside citation"}],"citations":[citation]}}]}},"stopReason":"end_turn","usage":{"inputTokens":11,"outputTokens":13,"totalTokens":24}})
}
fn anthropic_response(citation: Value) -> Value {
    json!({"id":"msg-source","type":"message","role":"assistant","model":"model","content":[{"type":"text","text":"Answer beside citation","citations":[citation]}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":13}})
}
#[test]
fn bedrock_search_source_crosses_to_anthropic_with_span_text_and_usage() {
    let body = serde_json::to_vec(&bedrock_response(bedrock_citation())).unwrap();
    let (whole, cap) = translate("anthropic", "bedrock", OpVerb::CHAT, &body);
    let out: Value = serde_json::from_slice(&whole.answer.body).unwrap();
    assert_eq!(
        out["content"][0],
        json!({"type":"text","text":"Answer beside citation","citations":[anthropic_citation()]})
    );
    assert_eq!(out["usage"]["input_tokens"], 11);
    assert_eq!(out["usage"]["output_tokens"], 13);
    assert!(whole.dropped.is_empty(), "{:?}", whole.dropped);
    assert!(!cap.contains("union"));
}
#[test]
fn anthropic_search_source_crosses_to_bedrock_without_union_conflict() {
    let body = serde_json::to_vec(&anthropic_response(anthropic_citation())).unwrap();
    let (whole, cap) = translate("bedrock", "anthropic", OpVerb::CHAT, &body);
    let out: Value = serde_json::from_slice(&whole.answer.body).unwrap();
    assert_eq!(
        out["output"]["message"]["content"][0],
        json!({"citationsContent":{"content":[{"text":"Answer beside citation"}],"citations":[bedrock_citation()]}})
    );
    assert_eq!(out["usage"]["inputTokens"], 11);
    assert_eq!(out["usage"]["outputTokens"], 13);
    assert!(whole.dropped.is_empty(), "{:?}", whole.dropped);
    assert!(!cap.contains("union"));
    assert!(!cap.contains("opaque:source#7"));
}
#[test]
fn native_images_and_citation_responses_retain_literal_bytes() {
    for (op, body) in [
        (OpVerb::IMAGE, b"{ \"images\" : [\"QQ==\"] }".to_vec()),
        (
            OpVerb::CHAT,
            serde_json::to_vec_pretty(&bedrock_response(bedrock_citation())).unwrap(),
        ),
    ] {
        let (whole, cap) = translate("bedrock", "bedrock", op, &body);
        assert_eq!(whole.answer.body, body);
        assert!(whole.dropped.is_empty());
        assert!(!cap.contains("ir.images[].url"));
    }
}
