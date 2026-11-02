//! Gemini structured output (`responseMimeType` / `responseSchema` / `responseJsonSchema`) and
//! the JSON-Schema sanitizing and `$ref` inlining Gemini's schema subset needs.

use super::*;
use crate::codec::keys;

/// Read Gemini's structured-output directive from `generationConfig` into [`crate::codec::ir::IrResponseFormat`].
/// The only code that knows this wire shape: `responseMimeType` (e.g. `"application/json"`) plus an
/// optional `responseSchema` (Gemini has no single `response_format` key). `None` when neither is
/// present, so a plain request gains no directive.
pub(super) fn read_gemini_response_format(
    gen_config: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let gc = gen_config?;
    let mime = gc.get(FIELD_RESPONSE_MIME_TYPE).and_then(|m| m.as_str());
    // Two schema slots: OpenAPI-subset `responseSchema` (normalized to JSON Schema, GEM-11) and
    // JSON-Schema `responseJsonSchema` (read as-is, GEM-04; ignoring it sent JSON mode to a foreign
    // target with no schema). The API accepts one or the other.
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
/// or tool `parameters`. Gemini accepts a strict OpenAPI 3.0 `Schema` subset, not full JSON Schema,
/// so draft keywords from OpenAI/Anthropic schemas hard-fail. One list so both sanitize identically.
///
/// Checked against the live API (2026-07-30), since the docs and backend disagree on `$ref`/`$defs`:
///
/// - Google's docs and announcement say `$ref`/`$defs`/`additionalProperties` are supported, but a
///   `$ref` into a named `$defs`/`definitions` entry (what Pydantic/Zod nested models emit) still
///   400s on the live backend (google-gemini/gemini-cli#13326, vercel/ai#14369; both fixed by
///   inlining client-side). The docs' own example is the root self-reference `"$ref": "#"`.
///   So `$ref`/`$defs`/`definitions` stay here: [`resolve_gemini_schema_refs`] runs first and
///   inlines every named `$ref`, leaving this filter only the unresolvable, dangling or cyclic cases
///   (see [`inline_gemini_schema_refs`]).
/// - `additionalProperties` is accepted only as a boolean; a schema value 400s with "Expected
///   boolean, received object" (google-gemini/gemini-cli#13694), the shape Pydantic `dict[str, Model]`
///   and Zod records emit. So [`sanitize_gemini_schema`] keeps it when boolean and strips it when a
///   schema, rather than listing it here.
/// - The rest (`$schema`, `$id`, `$comment`, `additionalItems`, `patternProperties`,
///   `unevaluatedProperties`, `const`, `examples`) are absent from the docs' keyword table (`type`,
///   `title`, `description`, `properties`, `required`, `additionalProperties`, `enum`, `format`,
///   `minimum`, `maximum`, `items`, `prefixItems`, `minItems`, `maxItems`, `anyOf`, `$ref`), which
///   warns not all JSON Schema is supported, and no evidence showed them accepted, so they stay.
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
/// cross-protocol tool / `responseSchema` does not 400. Returns a cleaned clone; the source IR value
/// is intact (only the egress wire copy is sanitized).
///
/// The filter is positional. Matching a key at every object level also fired inside a `properties`
/// map, where keys are caller-chosen field names: a property named `examples`, `const`,
/// `definitions` or `$ref` was deleted while `required` (never inspected) still named it, so Gemini
/// 400ed on a `required` entry with no property (or an optional field became invisible to the
/// model). Descending name-keyed maps via [`sanitize_gemini_schema_names`] keeps the filter on keywords.
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
    // Only schema positions contain types. Defaults, enums and examples are caller data.
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
                // `additionalProperties` is value-dependent: the live API accepts the boolean form but
                // 400s on the schema form (see the note on GEMINI_SCHEMA_REJECTED_KEYS).
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
/// and every value is sanitized as a schema. A non-object is malformed and falls back to the
/// keyword walker rather than being invented into a schema.
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

/// Collect every `$defs`/`definitions` map in a schema, keyed by definition name, for
/// [`inline_gemini_schema_refs`]. Positional like [`sanitize_gemini_schema`]: it does not descend
/// into instance data; it descends schema values of name maps without interpreting their keys
/// (a property named `definitions` must not be taken for a definitions map). An
/// earlier-collected name wins on collision, close enough to nearest-scope-wins for the generated
/// schemas this handles.
pub(super) fn collect_gemini_schema_defs(
    schema: &serde_json::Value,
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let serde_json::Value::Object(map) = schema {
        for defs_key in ["$defs", keys::DEFINITIONS] {
            if let Some(serde_json::Value::Object(defs)) = map.get(defs_key) {
                for (k, v) in defs {
                    out.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        for (key, value) in map {
            if key == "$defs" || key == keys::DEFINITIONS {
                continue;
            }
            match schema_children(key) {
                SchemaChildren::Names => {
                    if let Some(names) = value.as_object() {
                        for child in names.values() {
                            collect_gemini_schema_defs(child, out);
                        }
                    }
                }
                SchemaChildren::Many => match value.as_array() {
                    Some(children) => {
                        for child in children {
                            collect_gemini_schema_defs(child, out);
                        }
                    }
                    None => collect_gemini_schema_defs(value, out),
                },
                SchemaChildren::One => collect_gemini_schema_defs(value, out),
                SchemaChildren::Literal => {}
            }
        }
    }
}

/// Ceiling on nodes one `$ref` resolution may emit. The `active` stack stops self-reference but not a
/// cycle-free schema whose expansion is enormous: N chained definitions each referencing the next
/// twice inline to 2^N nodes. Client-supplied schemas reach this walk (tool `input_schema`,
/// `response_format`), so it needs its own ceiling.
pub(super) const GEMINI_SCHEMA_INLINE_MAX_NODES: usize = 100_000;

/// Ceiling on inlined nesting depth, matching the substrate JSON parser's input depth guard. That
/// guard sees the schema as it arrives; inlining can multiply it, so a few thousand chained shallow
/// definitions could build a `Value` deep enough to overflow the stack on the walk and on drop.
pub(super) const GEMINI_SCHEMA_INLINE_MAX_DEPTH: usize = 128;

/// One inlining allowance, owned by [`resolve_gemini_schema_refs`] and threaded through the walk so
/// every branch draws on the same pool.
pub(super) struct GeminiInlineBudget {
    remaining_nodes: usize,
    depth: usize,
}

/// Recursively inline every `$ref` pointing at a named `#/$defs/X` or `#/definitions/X` entry in
/// `defs`, replacing it with the resolved target. `active` is the stack of definitions being expanded
/// on this path: a recursive model has no finite inlining and the live backend does not reliably
/// resolve named `$defs` refs (see [`GEMINI_SCHEMA_REJECTED_KEYS`]), so a cycle falls back to an
/// untyped `{}` for that branch. A `$ref` that does not resolve (dangling, external URI, or root
/// `"$ref":"#"`) is left for [`sanitize_gemini_schema`]'s keyword filter. Positional like
/// [`collect_gemini_schema_defs`]: [`GEMINI_SCHEMA_NAME_KEYED_MAPS`] values are walked as
/// name-to-subschema maps.
///
/// `budget` bounds the expansion: exhausting the node pool or depth ceiling degrades to the same
/// `{}` a cycle does, instead of hanging or overflowing the stack.
pub(super) fn inline_gemini_schema_refs(
    schema: &serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    // Spending the budget at the one place every recursive edge passes means an exhausted branch
    // degrades to the untyped `{}` the cycle arm returns.
    if budget.remaining_nodes == 0 || budget.depth >= GEMINI_SCHEMA_INLINE_MAX_DEPTH {
        return serde_json::json!({});
    }
    budget.remaining_nodes -= 1;
    budget.depth += 1;
    let out = inline_gemini_schema_refs_within_budget(schema, defs, active, budget);
    budget.depth -= 1;
    out
}

/// Copy instance data without interpreting any member as a schema keyword. Each node still
/// spends the expansion allowance: repeated references must not clone a large literal for free.
fn inline_gemini_literal(
    value: &serde_json::Value,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    if budget.remaining_nodes == 0 || budget.depth >= GEMINI_SCHEMA_INLINE_MAX_DEPTH {
        return serde_json::json!({});
    }
    budget.remaining_nodes -= 1;
    budget.depth += 1;
    let out = match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, child)| (key.clone(), inline_gemini_literal(child, budget)))
                .collect(),
        ),
        serde_json::Value::Array(children) => serde_json::Value::Array(
            children
                .iter()
                .map(|child| inline_gemini_literal(child, budget))
                .collect(),
        ),
        other => other.clone(),
    };
    budget.depth -= 1;
    out
}

fn inline_gemini_child(
    key: &str,
    value: &serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    match schema_children(key) {
        SchemaChildren::Literal => inline_gemini_literal(value, budget),
        SchemaChildren::Names if !value.is_object() => inline_gemini_literal(value, budget),
        _ => map_schema_child(key, value, |child| {
            inline_gemini_schema_refs(child, defs, active, budget)
        }),
    }
}

/// The inlining walk proper, reached only through [`inline_gemini_schema_refs`], which charges the
/// node/depth budget for this node first.
pub(super) fn inline_gemini_schema_refs_within_budget(
    schema: &serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    active: &mut Vec<String>,
    budget: &mut GeminiInlineBudget,
) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(r)) = map.get("$ref") {
                let target_name = r
                    .strip_prefix("#/$defs/")
                    .or_else(|| r.strip_prefix("#/definitions/"));
                if let Some(name) = target_name {
                    if let Some(target) = defs.get(name) {
                        if active.contains(&name.to_string()) {
                            return serde_json::json!({});
                        }
                        active.push(name.to_string());
                        let inlined = inline_gemini_schema_refs(target, defs, active, budget);
                        active.pop();
                        // Sibling keywords beside `$ref` (e.g. a caller `description`) extend/override the target.
                        if map.len() > 1 {
                            if let serde_json::Value::Object(mut inlined_map) = inlined {
                                for (k, v) in map {
                                    if k != "$ref" {
                                        inlined_map.insert(
                                            k.clone(),
                                            inline_gemini_child(k, v, defs, active, budget),
                                        );
                                    }
                                }
                                return serde_json::Value::Object(inlined_map);
                            }
                        }
                        return inlined;
                    }
                }
            }
            let cleaned: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "$defs" && k.as_str() != keys::DEFINITIONS)
                .map(|(k, v)| {
                    let resolved = inline_gemini_child(k, v, defs, active, budget);
                    (k.clone(), resolved)
                })
                .collect();
            serde_json::Value::Object(cleaned)
        }
        other => other.clone(),
    }
}

/// Entry point: resolve every `$ref` against the schema's own `$defs`/`definitions` and drop those
/// maps, so a Pydantic/Zod nested-model schema reaches [`sanitize_gemini_schema`] flattened. Called
/// before it at both sites (`responseSchema` and tool `parameters`). A schema with no
/// `$defs`/`definitions` is returned unchanged (the common case).
pub(super) fn resolve_gemini_schema_refs(schema: &serde_json::Value) -> serde_json::Value {
    let mut defs = serde_json::Map::new();
    collect_gemini_schema_defs(schema, &mut defs);
    if defs.is_empty() {
        return schema.clone();
    }
    let mut active = Vec::new();
    let mut budget = GeminiInlineBudget {
        remaining_nodes: GEMINI_SCHEMA_INLINE_MAX_NODES,
        depth: 0,
    };
    inline_gemini_schema_refs(schema, &defs, &mut active, &mut budget)
}
