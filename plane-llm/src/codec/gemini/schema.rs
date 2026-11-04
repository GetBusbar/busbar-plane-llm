//! Gemini structured output (`responseMimeType` / `responseSchema` / `responseJsonSchema`) and
//! the JSON-Schema sanitizing and `$ref` inlining Gemini's schema subset needs.

use super::*;
use crate::codec::keys;

/// Read Gemini's structured-output directive from `generationConfig` into [`crate::codec::ir::IrResponseFormat`].
pub(super) fn read_gemini_response_format(
    gen_config: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let gc = gen_config?;
    let mime = gc.get(FIELD_RESPONSE_MIME_TYPE).and_then(|m| m.as_str());

    let schema = gc
        .get(FIELD_RESPONSE_SCHEMA)
        .map(gemini_openapi_schema_to_json_schema)
        .or_else(|| gc.get(FIELD_RESPONSE_JSON_SCHEMA).cloned());
    if mime.is_none() && schema.is_none() {
        return None;
    }
    let json = schema.is_some() || mime == Some(MIME_APPLICATION_JSON);
    Some(crate::codec::ir::IrResponseFormat::bare(json, schema))
}

/// Project [`crate::codec::ir::IrResponseFormat`] into a Gemini `generationConfig` map. The only
/// code that builds this wire shape: a JSON directive emits `responseMimeType:"application/json"`
/// plus the sanitized `responseSchema`; a non-JSON one emits nothing (Gemini defaults to text).
pub(super) fn write_gemini_response_format(
    gen_config: &mut serde_json::Map<String, serde_json::Value>,
    rf: &crate::codec::ir::IrResponseFormat,
) {
    if !rf.json {
        return;
    }
    gen_config.insert(
        FIELD_RESPONSE_MIME_TYPE.to_string(),
        serde_json::json!(MIME_APPLICATION_JSON),
    );
    if let Some(schema) = &rf.schema {
        gen_config.insert(
            FIELD_RESPONSE_SCHEMA.to_string(),
            sanitize_gemini_schema(&resolve_gemini_schema_refs(schema)),
        );
    }
}

/// JSON-Schema keywords Gemini's OpenAPI-subset validator rejects with a 400 in a `responseSchema`
pub(super) const GEMINI_SCHEMA_REJECTED_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$ref",
    "$defs",
    keys::DEFINITIONS,
    "additionalItems",
    "patternProperties",
    "unevaluatedProperties",
    "const",
    "examples",
    "$comment",
];

/// JSON-Schema keywords whose value is a map from a caller-chosen name to a subschema. The keys are
/// field names, not keywords, so [`GEMINI_SCHEMA_REJECTED_KEYS`] must not apply to them (see
/// [`sanitize_gemini_schema`]). (`$defs`, `definitions` and `patternProperties` are stripped whole,
/// so never reach the descent.)
pub(super) const GEMINI_SCHEMA_NAME_KEYED_MAPS: &[&str] = &[keys::PROPERTIES, "dependentSchemas"];

#[derive(Clone, Copy)]
enum SchemaChildren {
    Names,
    Many,
    One,
    Literal,
}

fn schema_children(key: &str) -> SchemaChildren {
    match key {
        keys::PROPERTIES
        | "dependentSchemas"
        | "$defs"
        | keys::DEFINITIONS
        | "patternProperties" => SchemaChildren::Names,
        "items" | "anyOf" | "oneOf" | "allOf" | "prefixItems" => SchemaChildren::Many,
        keys::ADDITIONAL_PROPERTIES
        | "additionalItems"
        | "unevaluatedProperties"
        | "unevaluatedItems"
        | "contains"
        | "propertyNames"
        | "not"
        | "if"
        | "then"
        | "else"
        | "contentSchema" => SchemaChildren::One,
        _ => SchemaChildren::Literal,
    }
}

/// Descend only schema positions. Defaults, enum values and examples remain opaque caller data.
fn map_schema_child(
    key: &str,
    value: &serde_json::Value,
    mut walk: impl FnMut(&serde_json::Value) -> serde_json::Value,
) -> serde_json::Value {
    match schema_children(key) {
        SchemaChildren::Names => match value.as_object() {
            Some(names) => serde_json::Value::Object(
                names
                    .iter()
                    .map(|(name, child)| (name.clone(), walk(child)))
                    .collect(),
            ),
            None => value.clone(),
        },
        SchemaChildren::Many => match value {
            serde_json::Value::Array(children) => {
                serde_json::Value::Array(children.iter().map(walk).collect())
            }
            _ => walk(value),
        },
        SchemaChildren::One => walk(value),
        SchemaChildren::Literal => value.clone(),
    }
}

/// Recursively strip the keywords Gemini rejects (`GEMINI_SCHEMA_REJECTED_KEYS`) from a schema so a
/// cross-protocol tool / `responseSchema` does not 400.
pub(super) fn sanitize_gemini_schema(schema: &serde_json::Value) -> serde_json::Value {
    let mut cleaned = sanitize_gemini_schema_keywords(schema);
    write_gemini_nullable_types(&mut cleaned);
    cleaned
}

fn write_gemini_nullable_types(schema: &mut serde_json::Value) {
    let Some(map) = schema.as_object_mut() else {
        return;
    };
    let nullable_type = map.get(keys::TYPE).and_then(|value| {
        let types = value.as_array()?;
        let [first, second] = types.as_slice() else {
            return None;
        };
        let non_null = match (first.as_str()?, second.as_str()?) {
            ("null", other) | (other, "null") => other,
            _ => return None,
        };
        match non_null {
            "string" => Some("STRING"),
            "number" => Some("NUMBER"),
            "integer" => Some("INTEGER"),
            "boolean" => Some("BOOLEAN"),
            "array" => Some("ARRAY"),
            "object" => Some("OBJECT"),
            _ => None,
        }
    });
    if let Some(ty) = nullable_type {
        if map
            .get(FIELD_NULLABLE)
            .is_none_or(|v| v == &serde_json::Value::Bool(true))
        {
            map.insert(keys::TYPE.to_string(), serde_json::json!(ty));
            map.insert(FIELD_NULLABLE.to_string(), serde_json::Value::Bool(true));
        }
    }

    for (key, value) in map {
        match schema_children(key) {
            SchemaChildren::Names => {
                if let Some(names) = value.as_object_mut() {
                    for child in names.values_mut() {
                        write_gemini_nullable_types(child);
                    }
                }
            }
            SchemaChildren::Many => match value.as_array_mut() {
                Some(children) => {
                    for child in children {
                        write_gemini_nullable_types(child);
                    }
                }
                None => write_gemini_nullable_types(value),
            },
            SchemaChildren::One => write_gemini_nullable_types(value),
            SchemaChildren::Literal => {}
        }
    }
}

fn sanitize_gemini_schema_keywords(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (k, v) in map {
                if k == keys::ADDITIONAL_PROPERTIES {
                    if matches!(v, serde_json::Value::Bool(_)) {
                        cleaned.insert(k.clone(), v.clone());
                    }
                    continue;
                }
                if GEMINI_SCHEMA_REJECTED_KEYS.contains(&k.as_str()) {
                    continue;
                }
                let sanitized = if GEMINI_SCHEMA_NAME_KEYED_MAPS.contains(&k.as_str()) {
                    sanitize_gemini_schema_names(v)
                } else {
                    map_schema_child(k, v, sanitize_gemini_schema_keywords)
                };
                cleaned.insert(k.clone(), sanitized);
            }
            serde_json::Value::Object(cleaned)
        }
        other => other.clone(),
    }
}

/// The name-keyed half of [`sanitize_gemini_schema`]: keys are kept verbatim (caller field names)
/// and every value is sanitized as a schema.
pub(super) fn sanitize_gemini_schema_names(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), sanitize_gemini_schema_keywords(v)))
                .collect(),
        ),
        other => sanitize_gemini_schema_keywords(other),
    }
}

/// Ceiling on nodes one `$ref` resolution may emit. The `active` stack stops self-reference but not a
/// cycle-free schema whose expansion is enormous: N chained definitions each referencing the next
/// twice inline to 2^N nodes.
pub(super) const GEMINI_SCHEMA_INLINE_MAX_NODES: usize = 100_000;

/// Cumulative allowance for copied string/key bytes and pointer decoding scratch. Node bounds
/// alone cannot constrain expansion of a large description through many references. Sanitizing
/// the bounded result only makes a constant-factor copy; JSON escaping adds at most six bytes per
/// input string byte. Charge before allocating, including temporary pointer lookup strings.
pub(super) const GEMINI_SCHEMA_INLINE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on inlined nesting depth, matching the substrate JSON parser's input depth guard. That
/// guard sees the schema as it arrives; inlining can multiply it, so a few thousand chained shallow
/// definitions could build a `Value` deep enough to overflow the stack on the walk and on drop.
pub(super) const GEMINI_SCHEMA_INLINE_MAX_DEPTH: usize = 128;

/// One inlining allowance, owned by [`resolve_gemini_schema_refs`] and threaded through the walk so
/// every branch draws on the same pool.
pub(super) struct GeminiInlineBudget {
    remaining_nodes: usize,
    remaining_bytes: usize,
    depth: usize,
    failed: bool,
}

impl GeminiInlineBudget {
    fn bytes(&mut self, count: usize) -> bool {
        if self.failed || count > self.remaining_bytes {
            self.failed = true;
            return false;
        }
        self.remaining_bytes -= count;
        true
    }

    fn node(&mut self) -> bool {
        if self.failed || self.remaining_nodes == 0 || self.depth >= GEMINI_SCHEMA_INLINE_MAX_DEPTH
        {
            self.failed = true;
            return false;
        }
        self.remaining_nodes -= 1;
        self.depth += 1;
        true
    }

    fn string(&mut self, value: &str) -> Option<String> {
        self.bytes(value.len()).then(|| value.to_owned())
    }
}

/// Inline local JSON Pointer references against the original document, preserving container paths
/// and escaped property names. The active stack holds decoded pointers, not bare definition names.
pub(super) fn inline_gemini_schema_refs(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    if !budget.node() {
        return serde_json::json!({});
    }
    let out = inline_gemini_schema_refs_within_budget(schema, root, active, budget);
    budget.depth -= 1;
    out
}

/// Copy instance data without interpreting any member as a schema keyword. Each node still
/// spends the expansion allowance: repeated references must not clone a large literal for free.
fn inline_gemini_literal(
    value: &serde_json::Value,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    if !budget.node() {
        return serde_json::json!({});
    }
    let out = match value {
        serde_json::Value::Object(map) => {
            let mut copied = serde_json::Map::new();
            for (key, child) in map {
                let Some(key) = budget.string(key) else { break };
                let child = inline_gemini_literal(child, budget);
                if budget.failed {
                    break;
                }
                copied.insert(key, child);
            }
            serde_json::Value::Object(copied)
        }
        serde_json::Value::Array(children) => {
            let mut copied = Vec::new();
            for child in children {
                let child = inline_gemini_literal(child, budget);
                if budget.failed {
                    break;
                }
                copied.push(child);
            }
            serde_json::Value::Array(copied)
        }
        serde_json::Value::String(value) => budget
            .string(value)
            .map_or(serde_json::Value::Null, serde_json::Value::String),
        other => other.clone(),
    };
    budget.depth -= 1;
    out
}

fn inline_gemini_child(
    key: &str,
    value: &serde_json::Value,
    root: &serde_json::Value,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    match (schema_children(key), value) {
        (SchemaChildren::Names, serde_json::Value::Object(names)) => {
            if !budget.node() {
                return serde_json::json!({});
            }
            let mut copied = serde_json::Map::new();
            for (name, child) in names {
                let Some(name) = budget.string(name) else {
                    break;
                };
                let child = inline_gemini_schema_refs(child, root, active, budget);
                if budget.failed {
                    break;
                }
                copied.insert(name, child);
            }
            budget.depth -= 1;
            serde_json::Value::Object(copied)
        }
        (SchemaChildren::Many, serde_json::Value::Array(children)) => {
            if !budget.node() {
                return serde_json::json!({});
            }
            let mut copied = Vec::new();
            for child in children {
                let child = inline_gemini_schema_refs(child, root, active, budget);
                if budget.failed {
                    break;
                }
                copied.push(child);
            }
            budget.depth -= 1;
            serde_json::Value::Array(copied)
        }
        (SchemaChildren::Literal | SchemaChildren::Names, _) => {
            inline_gemini_literal(value, budget)
        }
        _ => inline_gemini_schema_refs(value, root, active, budget),
    }
}

/// The inlining walk proper, reached only through [`inline_gemini_schema_refs`], which charges the
/// node/depth budget for this node first.
pub(super) fn inline_gemini_schema_refs_within_budget(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                let pointer = reference
                    .as_str()
                    .and_then(|reference| local_schema_pointer(reference, budget));
                let target = pointer.as_deref().and_then(|pointer| root.pointer(pointer));
                if let (Some(pointer), Some(target)) = (pointer, target) {
                    if active.contains(&pointer) {
                        budget.failed = true;
                        return serde_json::json!({});
                    }
                    active.push(pointer);
                    let inlined = inline_gemini_schema_refs(target, root, active, budget);
                    active.pop();
                    let mut siblings = serde_json::Map::new();
                    for (key, value) in map {
                        if matches!(key.as_str(), "$ref" | "$defs" | "definitions") {
                            continue;
                        }
                        let Some(key) = budget.string(key) else { break };
                        let value = inline_gemini_child(&key, value, root, active, budget);
                        if budget.failed {
                            break;
                        }
                        siblings.insert(key, value);
                    }
                    return intersect_gemini_schemas(
                        inlined,
                        serde_json::Value::Object(siblings),
                        budget,
                    );
                }
                // An unresolved reference must not become an unconstrained schema when the
                // sanitizer removes $ref. Refuse the whole projection instead.
                budget.failed = true;
                return serde_json::json!({});
            }
            let mut cleaned = serde_json::Map::new();
            for (key, value) in map {
                if matches!(key.as_str(), "$defs" | "definitions") {
                    continue;
                }
                let Some(key) = budget.string(key) else { break };
                let value = inline_gemini_child(&key, value, root, active, budget);
                if budget.failed {
                    break;
                }
                cleaned.insert(key, value);
            }
            serde_json::Value::Object(cleaned)
        }
        // Malformed schema primitives/arrays still use the bounded literal copier.
        other => inline_gemini_literal(other, budget),
    }
}

/// Entry point: inline references against the original document and remove definition containers.
/// Unsupported, recursive or over-budget expansion is an explicit unsatisfiable conjunction;
/// Gemini may reject that representation, but it must never receive a silently weakened schema.
pub(super) fn resolve_gemini_schema_refs(schema: &serde_json::Value) -> serde_json::Value {
    let mut budget = GeminiInlineBudget {
        remaining_nodes: GEMINI_SCHEMA_INLINE_MAX_NODES,
        remaining_bytes: GEMINI_SCHEMA_INLINE_MAX_BYTES,
        depth: 0,
        failed: false,
    };
    resolve_gemini_schema_refs_with_budget(schema, &mut budget)
}

fn resolve_gemini_schema_refs_with_budget(
    schema: &serde_json::Value,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    let resolved = inline_gemini_schema_refs(schema, schema, &mut Vec::new(), budget);
    if budget.failed {
        serde_json::json!({"allOf": [false]})
    } else {
        resolved
    }
}

/// Decode the URI fragment before JSON Pointer's own ~0/~1 decoding in Value::pointer.
/// Reject malformed escapes rather than accidentally resolving a different target.
fn local_schema_pointer(reference: &str, budget: &mut GeminiInlineBudget) -> Option<String> {
    let fragment = reference.strip_prefix('#')?;
    // One decoded buffer plus up to two replacement buffers per JSON Pointer token in
    // serde_json::Value::pointer. The sum of token lengths never exceeds fragment.len().
    if !budget.bytes(fragment.len().saturating_mul(3)) {
        return None;
    }
    let mut decoded = Vec::with_capacity(fragment.len());
    let mut bytes = fragment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    let pointer = String::from_utf8(decoded).ok()?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return None;
        }
    }
    Some(pointer)
}

/// Flatten conjunctions only when a keyword has an exact representation in Gemini's schema
/// subset. All other collisions retain both operands in allOf: provider refusal is preferable
/// to accepting values outside the caller's schema. In particular, closed object schemas and
/// tuple items cannot be combined by simply unioning their properties/items.
fn intersect_gemini_schemas(
    left: serde_json::Value,
    right: serde_json::Value,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    use serde_json::Value;
    if budget.failed {
        return Value::Null;
    }
    if left == right || right == serde_json::json!({}) || right == Value::Bool(true) {
        return left;
    }
    if left == serde_json::json!({}) || left == Value::Bool(true) {
        return right;
    }
    let (mut left, right) = match (left, right) {
        (Value::Object(left), Value::Object(right)) => (left, right),
        (left, right) => return gemini_schema_conjunction(left, right, budget),
    };
    let annotation = |key: &str| {
        matches!(
            key,
            "title" | "description" | "default" | "examples" | "$comment"
        )
    };
    if left.keys().all(|key| annotation(key)) || right.keys().all(|key| annotation(key)) {
        left.extend(right);
        return Value::Object(left);
    }
    // Inspect collisions before consuming either map. Unsupported merges retain the entire
    // conjunction, including constraints already examined, without cloning any string or tree.
    let independent = |key: &str| {
        matches!(
            key,
            "type"
                | "format"
                | "title"
                | "description"
                | "default"
                | "examples"
                | "$comment"
                | "enum"
                | "minimum"
                | "maximum"
                | "minLength"
                | "maxLength"
                | "pattern"
                | "minItems"
                | "maxItems"
                | "minProperties"
                | "maxProperties"
                | "required"
                | "properties"
                | "items"
                | "anyOf"
                | "propertyOrdering"
        )
    };
    let supported = left.keys().chain(right.keys()).all(|key| independent(key))
        && right.iter().all(|(key, value)| {
            let Some(previous) = left.get(key) else {
                return true;
            };
            previous == value
                || annotation(key)
                || match key.as_str() {
                    "minLength" | "minItems" | "minProperties" | "maxLength" | "maxItems"
                    | "maxProperties" => previous.as_u64().is_some() && value.as_u64().is_some(),
                    "minimum" | "maximum" => numeric_schema_order(previous, value).is_some(),
                    "required" => [previous, value].iter().all(|array| {
                        array
                            .as_array()
                            .is_some_and(|names| names.iter().all(Value::is_string))
                    }),
                    "properties" | "items" => previous.is_object() && value.is_object(),
                    _ => false,
                }
        });
    if !supported {
        return gemini_schema_conjunction(Value::Object(left), Value::Object(right), budget);
    }
    for (key, value) in right {
        let Some(previous) = left.remove(&key) else {
            left.insert(key, value);
            continue;
        };
        let combined = if previous == value || annotation(&key) {
            value
        } else {
            match key.as_str() {
                "minLength" | "minItems" | "minProperties" | "maxLength" | "maxItems"
                | "maxProperties" => {
                    let a = previous.as_u64().unwrap_or_default();
                    let b = value.as_u64().unwrap_or_default();
                    if (key.starts_with("min") && a < b) || (key.starts_with("max") && a > b) {
                        value
                    } else {
                        previous
                    }
                }
                "minimum" | "maximum" => {
                    let ordering = numeric_schema_order(&previous, &value)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if (key == "minimum" && ordering.is_lt())
                        || (key == "maximum" && ordering.is_gt())
                    {
                        value
                    } else {
                        previous
                    }
                }
                "required" => {
                    let (Value::Array(mut names), Value::Array(additions)) = (previous, value)
                    else {
                        budget.failed = true;
                        return Value::Null;
                    };
                    let mut seen: std::collections::BTreeSet<&str> =
                        names.iter().filter_map(Value::as_str).collect();
                    let retain: Vec<bool> = additions
                        .iter()
                        .map(|name| name.as_str().is_some_and(|name| seen.insert(name)))
                        .collect();
                    drop(seen);
                    names.extend(
                        additions
                            .into_iter()
                            .zip(retain)
                            .filter_map(|(name, keep)| keep.then_some(name)),
                    );
                    Value::Array(names)
                }
                "properties" => {
                    let (Value::Object(mut properties), Value::Object(additions)) =
                        (previous, value)
                    else {
                        budget.failed = true;
                        return Value::Null;
                    };
                    for (name, schema) in additions {
                        let combined = match properties.remove(&name) {
                            Some(old) => intersect_gemini_schemas(old, schema, budget),
                            None => schema,
                        };
                        if budget.failed {
                            return Value::Null;
                        }
                        properties.insert(name, combined);
                    }
                    Value::Object(properties)
                }
                "items" => intersect_gemini_schemas(previous, value, budget),
                _ => {
                    budget.failed = true;
                    return Value::Null;
                }
            }
        };
        if budget.failed {
            return Value::Null;
        }
        left.insert(key, combined);
    }
    Value::Object(left)
}

/// Construct an owned conjunction, never json!([left, right]), which serializes/copies Values.
fn gemini_schema_conjunction(
    left: serde_json::Value,
    right: serde_json::Value,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    if !budget.bytes("allOf".len()) || budget.remaining_nodes < 2 {
        budget.failed = true;
        return serde_json::Value::Null;
    }
    budget.remaining_nodes -= 2;
    serde_json::Value::Object(serde_json::Map::from_iter([(
        "allOf".to_owned(),
        serde_json::Value::Array(vec![left, right]),
    )]))
}

fn numeric_schema_order(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> Option<std::cmp::Ordering> {
    let (left, right) = (left.as_number()?, right.as_number()?);
    if !left.is_f64() && !right.is_f64() {
        let integer = |n: &serde_json::Number| {
            n.as_i64()
                .map(i128::from)
                .or_else(|| n.as_u64().map(i128::from))
        };
        Some(integer(left)?.cmp(&integer(right)?))
    } else if left.is_f64() && right.is_f64() {
        left.as_f64()?.partial_cmp(&right.as_f64()?)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "tests/schema_reference_tests.rs"]
mod schema_reference_tests;
