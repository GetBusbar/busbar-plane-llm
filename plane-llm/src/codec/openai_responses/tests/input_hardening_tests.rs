use super::*;

fn assert_ir_parse_reject(res: Result<crate::codec::ir::IrRequest, IrError>, ctx: &str) {
    let err = res.expect_err(ctx);
    assert_eq!(
        err.class,
        StatusClass::ClientError,
        "{ctx}: must be a client 400"
    );
    assert_eq!(
        err.provider_signal.as_deref(),
        Some(crate::codec::framework::SIGNAL_IR_PARSE),
        "{ctx}: must carry the ir_parse signal"
    );
}

#[test]
fn top_level_input_wrong_typed_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": 42}),
        serde_json::json!({"model": "x", "input": {"a": 1}}),
        serde_json::json!({"model": "x", "input": true}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a present non-string/non-array `input` must reject",
        );
    }
}

#[test]
fn string_array_and_instructions_only_input_stay_lenient() {
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": "hi"}))
        .expect("string input must parse");

    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": []}))
        .expect("empty array input must parse");

    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "instructions": "be nice", "input": serde_json::Value::Null}))
        .expect("null input with instructions must parse");
}

#[test]
fn per_message_wrong_typed_content_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": [{"type": "message", "role": "user", "content": 5}]}),
        serde_json::json!({"model": "x", "input": [{"type": "message", "role": "user", "content": {"a": 1}}]}),
        serde_json::json!({"model": "x", "input": [{"role": "user", "content": 9}]}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a present wrong-typed message content must reject",
        );
    }
}

#[test]
fn per_message_string_and_array_content_stay_lenient() {
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": [{"type": "message", "role": "user", "content": "hi"}]}))
        .expect("string message content must parse");
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}))
        .expect("array message content must parse");
}

#[test]
fn function_call_missing_id_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": [{"type": "function_call", "name": "f", "arguments": "{}"}]}),
        serde_json::json!({"model": "x", "input": [{"type": "function_call", "call_id": "", "name": "f", "arguments": "{}"}]}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a function_call item with no usable call_id must reject",
        );
    }
}

#[test]
fn function_call_with_id_parses() {
    let ir = ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": [
            {"type": "function_call", "call_id": "call_abc", "name": "f", "arguments": "{}"}
        ]}))
        .expect("a function_call with a non-empty call_id must parse");
    let has_id =
        ir.messages.iter().flat_map(|m| &m.content).any(
            |b| matches!(b, crate::codec::ir::IrBlock::ToolUse { id, .. } if id == "call_abc"),
        );
    assert!(has_id, "the call_id must be carried into the IR verbatim");
}

#[test]
fn top_level_tools_wrong_typed_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": "hi", "tools": {"a": 1}}),
        serde_json::json!({"model": "x", "input": "hi", "tools": "nope"}),
        serde_json::json!({"model": "x", "input": "hi", "tools": 7}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a present-but-wrong-typed tools must reject",
        );
    }
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": "hi"}))
        .expect("absent tools must stay lenient");
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": "hi", "tools": []}))
        .expect("empty-array tools must parse");
}

#[test]
fn stateful_previous_response_id_wrong_typed_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": "hi", "previous_response_id": 5}),
        serde_json::json!({"model": "x", "input": "hi", "previous_response_id": true}),
        serde_json::json!({"model": "x", "input": "hi", "previous_response_id": {"id": "resp_1"}}),
        serde_json::json!({"model": "x", "input": "hi", "previous_response_id": ["resp_1"]}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a present non-string previous_response_id must reject",
        );
    }
}

#[test]
fn stateful_store_wrong_typed_rejects() {
    for bad in [
        serde_json::json!({"model": "x", "input": "hi", "store": "yes"}),
        serde_json::json!({"model": "x", "input": "hi", "store": 1}),
        serde_json::json!({"model": "x", "input": "hi", "store": {"enabled": true}}),
    ] {
        assert_ir_parse_reject(
            ResponsesReader.read_request(&bad),
            "a present non-bool store must reject",
        );
    }
}

#[test]
fn stateful_knobs_stay_lenient_when_absent_null_or_correctly_typed() {
    ResponsesReader
        .read_request(&serde_json::json!({"model": "x", "input": "hi"}))
        .expect("absent stateful knobs must parse");

    ResponsesReader
        .read_request(&serde_json::json!({
            "model": "x", "input": "hi",
            "previous_response_id": serde_json::Value::Null,
            "store": serde_json::Value::Null
        }))
        .expect("null stateful knobs must parse");

    ResponsesReader
        .read_request(&serde_json::json!({
            "model": "x", "input": "hi",
            "previous_response_id": "resp_abc123",
            "store": false
        }))
        .expect("correctly-typed stateful knobs must parse");
}
