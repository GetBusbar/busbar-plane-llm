//! THE REGRESSION NET for the `response_format` bug class. Before the typed `IrResponseFormat`

use super::*;
use crate::codec::ir::{IrBlock, IrMessage, IrResponseFormat, IrRole};
use crate::codec_diagnostics::{diagnostic_texts, warnings, DiagnosticEvents};

fn req_with_format(rf: IrResponseFormat) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![IrBlock::text("hi".to_string())],
        }],
        response_format: Some(rf),
        ..Default::default()
    }
}

/// ONE typed directive, projected by EVERY native-structured-output writer, must land in THAT
#[test]
fn every_writer_emits_its_native_shape() {
    let schema = serde_json::json!({"type":"object","properties":{"x":{"type":"string"}}});
    let req = req_with_format(IrResponseFormat {
        json: true,
        schema: Some(schema),
        name: Some("out".to_string()),
        strict: None,
        description: None,
    });

    let o = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        o.pointer("/response_format/type"),
        Some(&serde_json::json!("json_schema"))
    );
    assert_eq!(
        o.pointer("/response_format/json_schema/schema/properties/x/type"),
        Some(&serde_json::json!("string"))
    );
    assert!(o.pointer("/response_format/json_schema/name").is_some());
    assert!(
        o.pointer("/response_format/responseMimeType").is_none(),
        "no Gemini-shaped key may leak into OpenAI: {o}"
    );

    let cohere_writer = CohereWriter;
    let c = cohere_writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        c.pointer("/response_format/type"),
        Some(&serde_json::json!("json_object"))
    );
    assert_eq!(
        c.pointer("/response_format/json_schema/properties/x/type"),
        Some(&serde_json::json!("string"))
    );
    assert!(
        c.pointer("/response_format/json_schema/schema").is_none(),
        "Cohere does NOT nest under .schema (that's OpenAI's shape): {c}"
    );

    let gemini_writer = GeminiWriter;
    let g = gemini_writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        g.pointer("/generationConfig/responseMimeType"),
        Some(&serde_json::json!("application/json"))
    );
    assert_eq!(
        g.pointer("/generationConfig/responseSchema/properties/x/type"),
        Some(&serde_json::json!("string"))
    );
    assert!(
        g.pointer("/response_format").is_none(),
        "Gemini has no top-level response_format: {g}"
    );

    let responses_writer = ResponsesWriter;
    let r = responses_writer.write_request(crate::codec::test_host::HOST, &req);
    assert_eq!(
        r.pointer("/text/format/type"),
        Some(&serde_json::json!("json_schema"))
    );
    assert_eq!(
        r.pointer("/text/format/schema/properties/x/type"),
        Some(&serde_json::json!("string"))
    );
    assert!(
        r.pointer("/text/format/json_schema").is_none(),
        "Responses text.format is FLAT, not nested under json_schema: {r}"
    );
}

/// And the read side: each protocol's NATIVE structured-output request canonicalizes into the
#[test]
fn every_reader_canonicalizes_to_typed_ir() {
    let oi = OpenAiReader
            .read_request(&serde_json::json!({
                "messages":[{"role":"user","content":"hi"}],
                "response_format":{"type":"json_schema","json_schema":{"name":"out","schema":{"type":"object"}}}
            }))
            .unwrap();
    let rf = oi.response_format.unwrap();
    assert!(rf.json && rf.name.as_deref() == Some("out") && rf.schema.is_some());

    let co = CohereReader
        .read_request(&serde_json::json!({
            "model":"command-r",
            "messages":[{"role":"user","content":"hi"}],
            "response_format":{"type":"json_object","json_schema":{"type":"object"}}
        }))
        .unwrap();
    let rf = co.response_format.unwrap();
    assert!(rf.json && rf.schema.is_some());

    let ge = GeminiReader
            .read_request(&serde_json::json!({
                "contents":[{"role":"user","parts":[{"text":"hi"}]}],
                "generationConfig":{"responseMimeType":"application/json","responseSchema":{"type":"object"}}
            }))
            .unwrap();
    let rf = ge.response_format.unwrap();
    assert!(rf.json && rf.schema.is_some());

    let re = ResponsesReader
        .read_request(&serde_json::json!({
            "input":"hi",
            "text":{"format":{"type":"json_schema","name":"out","schema":{"type":"object"}}}
        }))
        .unwrap();
    let rf = re.response_format.unwrap();
    assert!(rf.json && rf.name.as_deref() == Some("out") && rf.schema.is_some());
}

fn req_with_tool_choice_no_tools() -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        tools: vec![],
        tool_choice: Some(crate::codec::ir::IrToolChoice::Required),
        ..Default::default()
    }
}

/// A `tool_choice` with NO accompanying `tools` is a guaranteed 400 on every vendor that models the
#[test]
fn tool_choice_without_tools_is_omitted_on_every_writer() {
    let req = req_with_tool_choice_no_tools();
    let cohere_writer = CohereWriter;
    let gemini_writer = GeminiWriter;
    let responses_writer = ResponsesWriter;
    let bedrock_writer = BedrockWriter;

    let run = |f: &dyn Fn() -> serde_json::Value| -> (serde_json::Value, DiagnosticEvents) {
        let (cap, cap_subscriber) = warnings();
        let subscriber = cap_subscriber.clone();
        let value = tracing::dispatcher::with_default(&subscriber, f);
        (value, cap)
    };
    let (a, cap_a) = run(&|| anthropic_writer().write_request(crate::codec::test_host::HOST, &req));
    let (o, cap_o) = run(&|| openai_writer().write_request(crate::codec::test_host::HOST, &req));
    let (g, cap_g) = run(&|| gemini_writer.write_request(crate::codec::test_host::HOST, &req));
    let (b, cap_b) = run(&|| bedrock_writer.write_request(crate::codec::test_host::HOST, &req));
    let (c, cap_c) = run(&|| cohere_writer.write_request(crate::codec::test_host::HOST, &req));
    let (r, cap_r) = run(&|| responses_writer.write_request(crate::codec::test_host::HOST, &req));

    assert!(
        a.get("tool_choice").is_none(),
        "anthropic must omit tool_choice with no tools; got {a}"
    );
    assert!(
        o.get("tool_choice").is_none(),
        "openai must omit tool_choice with no tools; got {o}"
    );
    assert!(
        g.get("toolConfig")
            .and_then(|tc| tc.get("functionCallingConfig"))
            .is_none(),
        "gemini must omit functionCallingConfig with no tools; got {g}"
    );
    assert!(
        b.get("toolConfig")
            .and_then(|tc| tc.get("toolChoice"))
            .is_none(),
        "bedrock (regression proof) must omit toolChoice with no tools; got {b}"
    );
    assert!(
        c.get("tool_choice").is_none(),
        "cohere must omit tool_choice with no tools; got {c}"
    );
    assert!(
        r.get("tool_choice").is_none(),
        "responses must omit tool_choice with no tools; got {r}"
    );

    for (name, cap) in [
        ("Anthropic", &cap_a),
        ("OpenAI", &cap_o),
        ("Gemini", &cap_g),
        ("Cohere", &cap_c),
        ("Responses", &cap_r),
        ("Bedrock", &cap_b),
    ] {
        assert!(
            diagnostic_texts(cap)
                .iter()
                .any(|event| event.contains("dropping"))
                && (diagnostic_texts(cap)
                    .iter()
                    .any(|event| event.contains("tool_choice"))
                    || diagnostic_texts(cap)
                        .iter()
                        .any(|event| event.contains("toolChoice"))),
            "{name} must warn that it dropped the tool_choice; got {:?}",
            diagnostic_texts(cap)
        );
    }
}

fn req_with_stop(stop: Vec<String>) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        stop,
        ..Default::default()
    }
}

/// The IR's `stop` is an unbounded `Vec<String>` (no protocol enforces a cap on ingress), so a
#[test]
fn stop_sequences_clamped_per_vendor_cap() {
    let stops: Vec<String> = (0..8).map(|i| format!("STOP{i}")).collect();
    let req = req_with_stop(stops.clone());

    let openai_writer = OpenAiWriter;
    assert_eq!(
        crate::codec::decl_of("openai").and_then(|d| d.stop_sequence_cap),
        Some((4, "OpenAI")),
        "openai must publish its documented stop-sequence cap of 4"
    );
    let o = openai_writer.write_request(crate::codec::test_host::HOST, &req);
    let o_stop = o["stop"].as_array().expect("openai stop array");
    assert_eq!(
        o_stop.len(),
        4,
        "OpenAiWriter::write_request clamps an over-cap stop list to OpenAI's cap of 4 (v1.5.4 restore)"
    );

    let gemini_writer = GeminiWriter;
    assert_eq!(
        crate::codec::decl_of("gemini").and_then(|d| d.stop_sequence_cap),
        Some((5, "Gemini")),
        "gemini must publish its documented stop-sequence cap of 5"
    );
    let g = gemini_writer.write_request(crate::codec::test_host::HOST, &req);
    let g_stop = g["generationConfig"]["stopSequences"]
        .as_array()
        .expect("gemini stopSequences array");
    assert_eq!(
        g_stop.len(),
        5,
        "GeminiWriter::write_request clamps an over-cap stop list to Gemini's cap of 5 (v1.5.4 restore)"
    );

    assert_eq!(
        crate::codec::decl_of("cohere").and_then(|d| d.stop_sequence_cap),
        Some((5, "Cohere")),
        "cohere must publish its documented stop-sequence cap of 5"
    );

    let a = anthropic_writer().write_request(crate::codec::test_host::HOST, &req);
    let a_stop = a["stop_sequences"]
        .as_array()
        .expect("anthropic stop_sequences array");
    assert_eq!(
        a_stop.len(),
        8,
        "anthropic publishes no fixed stop-sequence cap; must NOT be clamped"
    );
}

/// Egress: an OpenAI/Anthropic caller who DID set `parallel_tool_calls` and routes to
#[test]
fn parallel_tool_calls_discarded_on_gemini_cohere_bedrock_warns() {
    let req = |parallel: Option<bool>| crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        parallel_tool_calls: parallel,
        ..Default::default()
    };

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let with_flag = req(Some(true));
    let gemini_writer = GeminiWriter;
    let cohere_writer = CohereWriter;
    let bedrock_writer = BedrockWriter;
    tracing::dispatcher::with_default(&subscriber, || {
        gemini_writer.write_request(crate::codec::test_host::HOST, &with_flag);
        cohere_writer.write_request(crate::codec::test_host::HOST, &with_flag);
        bedrock_writer.write_request(crate::codec::test_host::HOST, &with_flag);
    });
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("parallel_tool_calls"))
            && diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains("Gemini")),
        "gemini must warn on a carried parallel_tool_calls: {:?}",
        diagnostic_texts(&cap)
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("parallel_tool_calls"))
            && diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains("Cohere")),
        "cohere must warn on a carried parallel_tool_calls: {:?}",
        diagnostic_texts(&cap)
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("parallel_tool_calls"))
            && diagnostic_texts(&cap)
                .iter()
                .any(|event| event.contains("Bedrock")),
        "bedrock must warn on a carried parallel_tool_calls: {:?}",
        diagnostic_texts(&cap)
    );

    let (cap2, cap2_subscriber) = warnings();
    let subscriber2 = cap2_subscriber.clone();
    let without_flag = req(None);
    tracing::dispatcher::with_default(&subscriber2, || {
        gemini_writer.write_request(crate::codec::test_host::HOST, &without_flag);
        cohere_writer.write_request(crate::codec::test_host::HOST, &without_flag);
        bedrock_writer.write_request(crate::codec::test_host::HOST, &without_flag);
    });
    assert!(
        !diagnostic_texts(&cap2)
            .iter()
            .any(|event| event.contains("parallel_tool_calls")),
        "a request that never carried the flag must not warn about dropping it: {:?}",
        diagnostic_texts(&cap2)
    );
}

/// The production request translate steps onto `egress` (reader → egress seam → writer).
fn rehome_xreq(ingress: &'static str, egress: &str, body: &serde_json::Value) -> serde_json::Value {
    let ingress_p = crate::codec::proto_codec::protocol_for(ingress).expect("ingress");
    let egress_p = crate::codec::proto_codec::protocol_for(egress).expect("egress");
    let mut req = ingress_p.reader().read_request(body).expect("read_request");
    crate::codec::chat_handle::chat_prepare_for_egress(
        &mut req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: ingress,
            egress_requires_max_tokens: egress_p.decl().is_some_and(|d| d.requires_max_tokens),
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: true,
            reasoning_budgets: crate::codec::ir::REASONING_BUDGET_DEFAULTS,
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    egress_p
        .writer()
        .write_request(crate::codec::test_host::HOST, &req)
}

#[test]
fn openai_to_cohere_over_cap_stop_sequences_are_clamped_not_rejected() {
    let out = rehome_xreq(
        "openai",
        "cohere",
        &serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stop": ["a", "b", "c", "d", "e", "f"]
        }),
    );
    assert_eq!(
        out.get("stop_sequences"),
        Some(&serde_json::json!(["a", "b", "c", "d", "e"])),
        "an over-cap stop list is clamped to Cohere's cap of 5: {out}"
    );
}

#[test]
fn openai_to_cohere_exactly_cap_stop_sequences_are_forwarded_whole() {
    let out = rehome_xreq(
        "openai",
        "cohere",
        &serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stop": ["a", "b", "c", "d", "e"]
        }),
    );
    assert_eq!(
        out.get("stop_sequences"),
        Some(&serde_json::json!(["a", "b", "c", "d", "e"])),
        "an at-cap stop list is forwarded whole: {out}"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/stop_sequence_cap_degrade_tests.rs::openai_to_gemini_over_cap_stop_sequences_is_clamped_not_rejected`:
#[test]
fn openai_to_gemini_over_cap_stop_sequences_are_clamped_not_rejected() {
    let out = rehome_xreq(
        "openai",
        "gemini",
        &serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stop": ["a", "b", "c", "d", "e", "f"]
        }),
    );
    assert_eq!(
        out.pointer("/generationConfig/stopSequences"),
        Some(&serde_json::json!(["a", "b", "c", "d", "e"])),
        "an over-cap stop list is clamped to Gemini's cap of 5: {out}"
    );
}

/// Rehomes `crates/busbar-llm/src/engine/tests/stop_sequence_cap_degrade_tests.rs::anthropic_to_openai_over_cap_stop_sequences_is_clamped_not_rejected`:
#[test]
fn anthropic_to_openai_over_cap_stop_sequences_are_clamped_not_rejected() {
    let out = rehome_xreq(
        "anthropic",
        "openai",
        &serde_json::json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
            "stop_sequences": ["a", "b", "c", "d", "e"]
        }),
    );
    assert_eq!(
        out.get("stop"),
        Some(&serde_json::json!(["a", "b", "c", "d"])),
        "an over-cap stop list is clamped to OpenAI's cap of 4: {out}"
    );
}
