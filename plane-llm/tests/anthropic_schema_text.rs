//! Anthropic projections and malformed text at the actual reader, writer and attempt seams.

mod common;

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::codec::{decl_of, proto_codec::protocol_for};
use busbar_plane_llm::exchange::attempt::{translate_request, Translation};
use busbar_plane_llm::exchange::refuse::answered;
use busbar_plane_llm::exchange::shaping::FarShape;
use common::SevensHost;
use serde_json::{json, Value};

fn hop<'a>(ingress: &str, egress: &'static str, body: &Value, raw: &'a [u8]) -> Translation<'a> {
    translate_request(
        &SevensHost,
        ingress,
        OpVerb::CHAT,
        FarShape {
            dialect: egress,
            wire_model: "m",
            default_max_tokens: Some(100),
            prompt_caching: false,
            caps: Default::default(),
            path_base: None,
        },
        100,
        [1024, 4096, 8192, 16384],
        false,
        Some(body),
        "application/json",
        raw,
    )
}

fn openai_request(function: Value) -> Value {
    json!({"model":"m","messages":[{"role":"user","content":"hi"}],
        "tools":[{"type":"function","function":function}]})
}

fn schemas() -> Value {
    // The committed primary specification, SHA-256 d1d189d791d1b551b33edebe9a60d63836d167070863ef366969e0df1e9c4f55.
    serde_json::from_str(include_str!(
        "../../testing/llm-conformance/specs/anthropic-e50cf35b.yml"
    ))
    .expect("the pinned Anthropic specification is JSON")
}

#[test]
fn absent_openai_parameters_reach_anthropic_as_an_empty_object_schema() {
    // OpenAI's pinned FunctionParameters says omission defines an empty parameter list;
    // Anthropic's pinned InputSchema requires an object with type="object".
    let body = openai_request(json!({"name":"ping"}));
    let ir = protocol_for("openai")
        .unwrap()
        .reader()
        .read_request(&body)
        .expect("OpenAI permits omitted parameters");
    let expected = json!({"type":"object","properties":{}});
    assert_eq!(ir.tools[0].input_schema, expected);
    let wire = protocol_for("anthropic")
        .unwrap()
        .writer()
        .write_request(&SevensHost, &ir);
    assert_eq!(wire.pointer("/tools/0/input_schema"), Some(&expected));

    let raw = serde_json::to_vec(&body).unwrap();
    let translated = hop("openai", "anthropic", &body, &raw);
    assert!(translated.crossed);
    let out = translated.outcome.expect("the production hop translates");
    let out: Value = serde_json::from_slice(&out.bytes).unwrap();
    assert_eq!(out.pointer("/tools/0/input_schema"), Some(&expected));
    assert_eq!(
        schemas()["components"]["schemas"]["InputSchema"]["required"],
        json!(["type"])
    );
}

#[test]
fn present_openai_schemas_are_preserved_instead_of_normalized_as_absent() {
    let valid = json!({"type":"object","properties":{"city":{"type":"string"}},
        "required":["city"],"additionalProperties":false,"description":"keep me"});
    // Presence is decisive even for malformed values. This absence-only repair does not
    // globally tighten OpenAI ingress or silently substitute a schema for explicit null.
    for schema in [
        valid.clone(),
        json!({}),
        Value::Null,
        json!(123),
        json!(false),
        json!("bad"),
        json!([]),
    ] {
        let body = openai_request(json!({"name":"weather","parameters":schema}));
        let ir = protocol_for("openai")
            .unwrap()
            .reader()
            .read_request(&body)
            .expect("retain existing present-schema reader behavior");
        assert_eq!(ir.tools[0].input_schema, schema);
    }
    let body = openai_request(json!({"name":"weather","parameters":valid}));
    let raw = serde_json::to_vec(&body).unwrap();
    let translated = hop("openai", "anthropic", &body, &raw);
    let out = translated.outcome.expect("valid schema translates");
    let out: Value = serde_json::from_slice(&out.bytes).unwrap();
    assert_eq!(out.pointer("/tools/0/input_schema"), Some(&valid));
}

#[test]
fn openai_schema_alias_and_parameters_precedence_keep_present_values() {
    let alias = json!({"type":"object","properties":{"alias":{"type":"boolean"}}});
    let primary = json!({"type":"object","properties":{"primary":{"type":"string"}}});
    for (function, expected) in [
        (json!({"name":"ping","input_schema":alias}), alias.clone()),
        (json!({"name":"ping","input_schema":null}), Value::Null),
        (
            json!({"name":"ping","parameters":primary,"input_schema":alias}),
            primary,
        ),
        (
            json!({"name":"ping","parameters":null,"input_schema":alias}),
            Value::Null,
        ),
    ] {
        let body = openai_request(function);
        let ir = protocol_for("openai")
            .unwrap()
            .reader()
            .read_request(&body)
            .expect("the existing schema precedence remains accepted");
        assert_eq!(ir.tools[0].input_schema, expected);
    }
}

#[test]
fn native_openai_omitted_and_explicit_parameters_keep_the_callers_bytes() {
    for raw in [
        br#"{ "model":"m", "messages":[], "tools":[{"type":"function","function":{"name":"ping"}}] }"#.as_slice(),
        br#"{ "model":"m", "messages":[], "tools":[{"type":"function","function":{"name":"ping","parameters":null}}] }"#.as_slice(),
    ] {
        let body: Value = serde_json::from_slice(raw).unwrap();
        let translated = hop("openai", "openai", &body, raw);
        assert!(!translated.crossed);
        let out = translated.outcome.expect("native relay does not read tools");
        assert!(out.pristine);
        assert_eq!(out.bytes, raw);
    }
}

fn anthropic_request(block: Value, site: &str) -> Value {
    let mut body =
        json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"hi"}]});
    match site {
        "system" => body["system"] = json!([block]),
        "message" => body["messages"][0]["content"] = json!([block]),
        "tool_result" => {
            body["messages"][0]["content"] = json!([
                {"type":"tool_result","tool_use_id":"toolu_1","content":[block]}
            ])
        }
        _ => panic!("unknown test site"),
    }
    body
}

#[test]
fn present_wrong_typed_anthropic_text_is_a_native_400_before_foreign_egress() {
    for site in ["system", "message", "tool_result"] {
        for text in [
            Value::Null,
            json!(123),
            json!(true),
            json!({"lost":"content"}),
            json!(["lost"]),
        ] {
            let body = anthropic_request(json!({"type":"text","text":text}), site);
            let error = protocol_for("anthropic")
                .unwrap()
                .reader()
                .read_request(&body)
                .expect_err("present wrong-typed text cannot become empty content");
            assert_eq!(error.provider_signal.as_deref(), Some("ir_parse"));

            let raw = serde_json::to_vec(&body).unwrap();
            let translated = hop("anthropic", "openai", &body, &raw);
            let error = translated
                .outcome
                .expect_err("nothing can go to the foreign lane");
            assert_eq!(error.status, 400, "{site}: {text}");
            assert_eq!(error.envelope, "anthropic");
            let refusal = answered(&SevensHost, &error);
            assert_eq!(refusal.status, 400);
            let wire: Value = serde_json::from_slice(&refusal.body).unwrap();
            assert_eq!(wire["type"], "error");
            assert_eq!(wire["error"]["type"], "invalid_request_error");
            assert_eq!(
                wire["error"]["message"],
                "We could not process the content of your request."
            );
            let request_id = wire["request_id"].as_str().expect("native request id");
            assert!(refusal
                .fields
                .iter()
                .any(|(name, value)| name == "request-id" && value == request_id.as_bytes()));
        }
    }
}

#[test]
fn absent_and_string_anthropic_text_keep_existing_reader_semantics() {
    for site in ["system", "message", "tool_result"] {
        for block in [
            json!({"type":"text"}),
            json!({"type":"text","text":""}),
            json!({"type":"text","text":"keep this"}),
        ] {
            let body = anthropic_request(block, site);
            protocol_for("anthropic")
                .unwrap()
                .reader()
                .read_request(&body)
                .expect("missing text remains lenient and string text remains valid");
            let raw = serde_json::to_vec(&body).unwrap();
            hop("anthropic", "openai", &body, &raw)
                .outcome
                .expect("the accepted shape still translates");
        }
    }
    let body = anthropic_request(json!({"type":"text","text":"keep this"}), "message");
    let raw = serde_json::to_vec(&body).unwrap();
    let translated = hop("anthropic", "openai", &body, &raw).outcome.unwrap();
    let wire: Value = serde_json::from_slice(&translated.bytes).unwrap();
    assert_eq!(
        wire["messages"][0]["content"],
        json!([{"type":"text","text":"keep this"}])
    );
}

#[test]
fn native_anthropic_wrong_typed_text_still_relays_byte_for_byte() {
    let raw = br#"{ "model":"m", "max_tokens":100, "messages":[{"role":"user","content":[{"type":"text","text":{"preserve":"native"}}]}] }"#;
    let body: Value = serde_json::from_slice(raw).unwrap();
    let translated = hop("anthropic", "anthropic", &body, raw);
    assert!(!translated.crossed);
    let out = translated
        .outcome
        .expect("native relay does not invoke the IR reader");
    assert!(out.pristine);
    assert_eq!(out.bytes.as_ref(), raw.as_slice());
}

#[test]
fn anthropic_response_text_rejects_present_wrong_types_and_keeps_absent_or_strings() {
    for text in [
        Value::Null,
        json!(123),
        json!(true),
        json!({"lost":"content"}),
        json!(["lost"]),
    ] {
        let body = json!({"role":"assistant","content":[{"type":"text","text":text}],
            "usage":{"input_tokens":7,"output_tokens":9}});
        let error = protocol_for("anthropic")
            .unwrap()
            .reader()
            .read_response(&SevensHost, &body)
            .expect_err("malformed upstream text cannot become a successful empty answer");
        assert_eq!(error.provider_signal.as_deref(), Some("ir_parse"));
    }
    for (block, expected) in [
        (json!({"type":"text"}), ""),
        (json!({"type":"text","text":""}), ""),
        (json!({"type":"text","text":"keep this"}), "keep this"),
    ] {
        let body = json!({"role":"assistant","content":[block],
            "usage":{"input_tokens":7,"output_tokens":9}});
        let ir = protocol_for("anthropic")
            .unwrap()
            .reader()
            .read_response(&SevensHost, &body)
            .expect("accepted response text keeps existing semantics");
        assert_eq!(
            ir.content,
            vec![busbar_plane_llm::codec::ir::IrBlock::text(expected)]
        );
        assert_eq!((ir.usage.input_tokens, ir.usage.output_tokens), (7, 9));
    }
}

#[test]
fn anthropic_model_list_emits_required_nullable_unknown_metadata() {
    let envelope = decl_of("anthropic").unwrap().models_list_envelope.unwrap();
    let spec = schemas();
    let required = spec["components"]["schemas"]["ModelInfo"]["required"]
        .as_array()
        .unwrap();
    let wire = envelope(&["first", "last"]);
    assert_eq!(wire["has_more"], false);
    assert_eq!(wire["first_id"], "first");
    assert_eq!(wire["last_id"], "last");
    for (model, name) in wire["data"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["first", "last"])
    {
        for key in required {
            assert!(
                model.get(key.as_str().unwrap()).is_some(),
                "required ModelInfo member {key}: {model}"
            );
        }
        for key in ["capabilities", "max_input_tokens", "max_tokens"] {
            assert_eq!(
                model.get(key),
                Some(&Value::Null),
                "unknown {key} must be explicit null"
            );
        }
        assert_eq!(model["id"], name);
        assert_eq!(model["display_name"], name);
        assert_eq!(model["type"], "model");
        assert_eq!(model["created_at"], "1970-01-01T00:00:00Z");
    }
    assert_eq!(
        envelope(&[]),
        json!({"data":[],"has_more":false,"first_id":null,"last_id":null})
    );
}
