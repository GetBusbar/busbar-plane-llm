// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::super::*;
use super::{inline_gemini_schema_refs, GeminiInlineBudget};
use crate::codec::proto_codec::protocol_for;
use serde_json::{json, Value};

fn assert_projection(schema: Value, expected: Value) {
    let mut request = protocol_for("openai")
        .unwrap()
        .reader()
        .read_request(&json!({
            "model": "test", "messages": [{"role": "user", "content": "hello"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "result", "schema": schema}},
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": schema}}]
        }))
        .expect("schema request");
    request.extra.clear();
    let writer = GeminiWriter;
    let wire = writer.write_request(crate::codec::test_host::HOST, &request);
    for path in [
        "/generationConfig/responseSchema",
        "/tools/0/functionDeclarations/0/parameters",
    ] {
        assert_eq!(wire.pointer(path), Some(&expected), "{path}");
    }
    assert_eq!(request.tools[0].input_schema, schema);
}

#[test]
fn reference_container_and_nested_paths_do_not_collide() {
    assert_projection(
        json!({
            "$defs": {"X": {"type": "string"}, "Nested": {"$defs": {"X": {"type": "boolean"}}}},
            "definitions": {"X": {"type": "integer"}},
            "type": "object", "properties": {
                "a": {"$ref": "#/$defs/X"}, "b": {"$ref": "#/definitions/X"},
                "c": {"$ref": "#/$defs/Nested/$defs/X"}
            }
        }),
        json!({"type": "object", "properties": {
            "a": {"type": "string"}, "b": {"type": "integer"}, "c": {"type": "boolean"}
        }}),
    );
}

#[test]
fn reference_pointer_and_uri_fragment_escaping_are_decoded_in_order() {
    assert_projection(
        json!({"$defs": {
            "a/b": {"type": "string"}, "a~b": {"type": "integer"},
            "a~1b": {"type": "boolean"}, "a b": {"type": "number"}
        }, "properties": {
            "slash": {"$ref": "#/$defs/a~1b"}, "tilde": {"$ref": "#/$defs/a~0b"},
            "literal": {"$ref": "#/$defs/a~01b"}, "space": {"$ref": "#/$defs/a%20b"}
        }}),
        json!({"properties": {
            "slash": {"type": "string"}, "tilde": {"type": "integer"},
            "literal": {"type": "boolean"}, "space": {"type": "number"}
        }}),
    );
}

#[test]
fn reference_sibling_bounds_intersect_in_both_directions() {
    for (target, sibling) in [(5, 2), (2, 5)] {
        assert_projection(
            json!({"$defs": {"S": {"type": "string", "minLength": target, "maxLength": 20}},
                "$ref": "#/$defs/S", "minLength": sibling, "maxLength": 10}),
            json!({"type": "string", "minLength": 5, "maxLength": 10}),
        );
    }
    assert_projection(
        json!({"$defs": {"N": {"type": "integer", "minimum": 9007199254740993_u64}},
            "$ref": "#/$defs/N", "minimum": 9007199254740992_u64}),
        json!({"type": "integer", "minimum": 9007199254740993_u64}),
    );
}

#[test]
fn reference_sibling_properties_and_required_intersect() {
    assert_projection(
        json!({"$defs": {"O": {"type": "object", "required": ["a"],
            "properties": {"a": {"type": "string", "minLength": 5}}}},
            "$ref": "#/$defs/O", "required": ["b", "a"],
            "properties": {"a": {"minLength": 2}, "b": {"type": "integer"}}}),
        json!({"type": "object", "required": ["a", "b"],
            "properties": {"a": {"type": "string", "minLength": 5}, "b": {"type": "integer"}}}),
    );
}

#[test]
fn unrepresentable_intersections_preserve_both_constraints() {
    for (target, sibling) in [
        (
            json!({"type": "string", "pattern": "^a"}),
            json!({"pattern": "z$"}),
        ),
        (json!({"type": "string"}), json!({"type": "integer"})),
        (
            json!({"properties": {"a": {"type": "string"}}, "additionalProperties": false}),
            json!({"properties": {"b": {"type": "string"}}}),
        ),
        (
            json!({"if": {"type": "string"}}),
            json!({"then": {"minLength": 5}}),
        ),
    ] {
        let mut schema = sibling.clone();
        schema["$defs"] = json!({"T": target});
        schema["$ref"] = json!("#/$defs/T");
        assert_projection(schema, json!({"allOf": [target, sibling]}));
    }
}

#[test]
fn references_inside_instance_data_remain_opaque() {
    let literal = json!({"$ref": "#/$defs/Missing", "$defs": {"S": {"type": "integer"}}});
    assert_projection(
        json!({"$defs": {"S": {"type": "object", "default": literal, "enum": [literal]}},
            "$ref": "#/$defs/S"}),
        json!({"type": "object", "default": literal, "enum": [literal]}),
    );
}

#[test]
fn unresolved_recursive_and_invalid_references_are_not_erased() {
    for reference in [
        "#/$defs/Missing",
        "https://example.invalid/schema",
        "#/$defs/bad~2",
        "#/$defs/%GG",
        "#",
    ] {
        assert_projection(json!({"$ref": reference}), json!({"allOf": [false]}));
    }
    assert_projection(
        json!({"$defs": {"A": {"$ref": "#/$defs/B"}, "B": {"$ref": "#/$defs/A"}}, "$ref": "#/$defs/A"}),
        json!({"allOf": [false]}),
    );
}

#[test]
fn exhausted_literal_or_schema_budget_refuses_the_entire_projection() {
    let schema = json!({"default": [1, 2, 3]});
    let mut budget = GeminiInlineBudget {
        remaining_nodes: 2,
        remaining_bytes: super::GEMINI_SCHEMA_INLINE_MAX_BYTES,
        depth: 0,
        failed: false,
    };
    let _ = inline_gemini_schema_refs(&schema, &schema, &mut Vec::new(), &mut budget);
    assert!(budget.failed);
    let schema = json!({"$defs": {"A": {"type": "string"}}, "$ref": "#/$defs/A"});
    let mut budget = GeminiInlineBudget {
        remaining_nodes: 1,
        remaining_bytes: super::GEMINI_SCHEMA_INLINE_MAX_BYTES,
        depth: 0,
        failed: false,
    };
    let _ = inline_gemini_schema_refs(&schema, &schema, &mut Vec::new(), &mut budget);
    assert!(budget.failed);
}

#[test]
fn annotation_siblings_preserve_closed_object_projection() {
    assert_projection(
        json!({"$defs": {"O": {"type": "object", "properties": {}, "additionalProperties": false}},
            "$ref": "#/$defs/O", "description": "local description"}),
        json!({"type": "object", "properties": {}, "additionalProperties": false,
            "description": "local description"}),
    );
}

fn byte_budget(bytes: usize) -> GeminiInlineBudget {
    GeminiInlineBudget {
        remaining_nodes: 1_000,
        remaining_bytes: bytes,
        depth: 0,
        failed: false,
    }
}

#[test]
fn schema_byte_budget_charges_strings_and_all_name_maps_before_copying() {
    for schema in [
        json!({"description": "x".repeat(64)}),
        json!({"properties": {"x".repeat(64): {"type": "string"}}}),
        json!({"default": {"x".repeat(64): 1}}),
        json!({"default": ["x".repeat(64)]}),
        json!({"x".repeat(64): true}),
        json!("x".repeat(64)),
        json!(["x".repeat(64)]),
    ] {
        let mut budget = byte_budget(32);
        let output = super::resolve_gemini_schema_refs_with_budget(&schema, &mut budget);
        assert!(budget.failed);
        assert_eq!(output, json!({"allOf": [false]}));
        assert!(
            budget.remaining_nodes > 900,
            "byte bound, not node exhaustion"
        );
    }
}

#[test]
fn repeated_reference_descriptions_share_one_byte_allowance() {
    let schema = json!({
        "$defs": {"S": {"type": "string", "description": "x".repeat(64)}},
        "properties": {"a": {"$ref": "#/$defs/S"}, "b": {"$ref": "#/$defs/S"}}
    });
    let mut budget = byte_budget(180);
    let output = super::resolve_gemini_schema_refs_with_budget(&schema, &mut budget);
    assert!(budget.failed);
    assert_eq!(output, json!({"allOf": [false]}));
    assert!(budget.remaining_nodes > 900);
    let mut sufficient = byte_budget(300);
    let output = super::resolve_gemini_schema_refs_with_budget(&schema, &mut sufficient);
    assert!(!sufficient.failed);
    assert_eq!(
        output.pointer("/properties/a/description"),
        Some(&json!("x".repeat(64)))
    );
    assert_eq!(
        output.pointer("/properties/b/description"),
        Some(&json!("x".repeat(64)))
    );
}

#[test]
fn reference_pointer_scratch_is_reserved_before_decoding_or_lookup() {
    let pointer = format!("#/$defs/{}", "x".repeat(64));
    let mut budget = byte_budget(32);
    assert!(super::local_schema_pointer(&pointer, &mut budget).is_none());
    assert!(budget.failed);
    assert_eq!(
        budget.remaining_bytes, 32,
        "failed reservation does not underflow"
    );
    let mut budget = byte_budget(30);
    assert_eq!(
        super::local_schema_pointer("#/a~1b", &mut budget).as_deref(),
        Some("/a~1b")
    );
    assert_eq!(
        budget.remaining_bytes, 15,
        "decode and both pointer token replacements are charged"
    );
}

#[test]
fn schema_intersection_moves_owned_strings_without_copying_them() {
    let mut budget = byte_budget(0);
    let left = json!({"type": "string", "description": "x".repeat(64), "minLength": 5});
    let original_allocation = left["description"].as_str().unwrap().as_ptr();
    let output = super::intersect_gemini_schemas(left, json!({"minLength": 2}), &mut budget);
    assert_eq!(
        output["description"].as_str().unwrap().as_ptr(),
        original_allocation,
        "intersection transfers the existing allocation instead of cloning its bytes"
    );
    assert!(!budget.failed);
    assert_eq!(
        output,
        json!({"type": "string", "description": "x".repeat(64), "minLength": 5})
    );
    assert_eq!(budget.remaining_bytes, 0);
}

#[test]
fn intersection_conjunction_reserves_its_new_key_and_nodes() {
    let mut budget = byte_budget(4);
    let output = super::intersect_gemini_schemas(
        json!({"type": "string"}),
        json!({"type": "integer"}),
        &mut budget,
    );
    assert!(budget.failed);
    assert!(output.is_null());
    let mut budget = byte_budget(5);
    let output = super::intersect_gemini_schemas(
        json!({"type": "string"}),
        json!({"type": "integer"}),
        &mut budget,
    );
    assert!(!budget.failed);
    assert_eq!(budget.remaining_bytes, 0);
    assert_eq!(budget.remaining_nodes, 998);
    assert_eq!(
        output,
        json!({"allOf": [{"type": "string"}, {"type": "integer"}]})
    );
}

#[test]
fn large_required_intersections_keep_order_and_deduplicate() {
    let left: Vec<_> = (0..20_000).map(|i| format!("l{i}")).collect();
    let right: Vec<_> = (0..20_000).map(|i| format!("r{i}")).collect();
    let expected: Vec<_> = left.iter().chain(&right).cloned().collect();
    let schema = json!({"$defs":{"Base":{"required":left}},"$ref":"#/$defs/Base","required":right});
    let mut budget = GeminiInlineBudget {
        remaining_nodes: super::GEMINI_SCHEMA_INLINE_MAX_NODES,
        remaining_bytes: super::GEMINI_SCHEMA_INLINE_MAX_BYTES,
        depth: 0,
        failed: false,
    };
    let expanded = inline_gemini_schema_refs(&schema, &schema, &mut Vec::new(), &mut budget);
    assert!(!budget.failed);
    assert_eq!(expanded.get("required"), Some(&json!(expected)));
}

#[test]
fn malformed_required_intersections_retain_conjunction() {
    let schema = json!({"$defs":{"Base":{"required":["a"]}},"$ref":"#/$defs/Base","required":[7]});
    let mut budget = GeminiInlineBudget {
        remaining_nodes: super::GEMINI_SCHEMA_INLINE_MAX_NODES,
        remaining_bytes: super::GEMINI_SCHEMA_INLINE_MAX_BYTES,
        depth: 0,
        failed: false,
    };
    let expanded = inline_gemini_schema_refs(&schema, &schema, &mut Vec::new(), &mut budget);
    assert!(!budget.failed);
    assert_eq!(
        expanded.get("allOf"),
        Some(&json!([{"required":["a"]},{"required":[7]}]))
    );
}
