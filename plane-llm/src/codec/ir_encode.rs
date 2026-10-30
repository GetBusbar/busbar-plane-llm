// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR/wire projection helpers shared across the LLM dialect codecs: the image-source pair
//! (`image_url` string and IR, both ways), structured-json tool-result detection, and the
//! `tools[].strict` drop warning. They name concrete IR, so they sit beside the codecs that use them.

/// Reconstruct an OpenAI/Responses `image_url` string from an [`crate::codec::ir::IrImageSource`],
/// inverse of `parse_image_url`. A `Url` is verbatim; a `Base64` is re-wrapped as
/// `data:<mime>;base64,<payload>`. A `Vendor` reference has no projection (`None`; the caller drops
/// the block with a warn). Shared by the Chat and Responses writers.
pub fn image_url_from_ir(source: &crate::codec::ir::IrImageSource) -> Option<String> {
    match source {
        crate::codec::ir::IrImageSource::Url(url) => Some(url.clone()),
        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
            Some(format!("data:{media_type};base64,{data}"))
        }
        // An opaque vendor reference has no neutral `image_url` projection.
        crate::codec::ir::IrImageSource::Vendor { .. } => None,
    }
}

/// Decompose an OpenAI/Responses `image_url` string into the IR image source, inverse of
/// [`image_url_from_ir`]; shared by the Chat and Responses readers. A `data:<mime>;base64,<payload>`
/// URI splits into its MIME type ("image/png") and base64 payload, as the Anthropic reader/writer do
/// for base64 images. Any other URL (https, or a data URI we cannot confidently split) is kept
/// verbatim as a URL reference so a same-protocol round trip reproduces it exactly.
pub fn parse_image_url(url: &str) -> crate::codec::ir::IrImageSource {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, payload)) = rest.split_once(',') {
            // meta is e.g. "image/png;base64" or "image/png": keep only the MIME type.
            let media_type = meta.split(';').next().unwrap_or("").to_string();
            if meta.contains("base64") && !media_type.is_empty() {
                return crate::codec::ir::IrImageSource::Base64 {
                    media_type,
                    data: payload.to_string(),
                };
            }
        }
    }
    // Non-data URL or an unrecognized data URI: kept verbatim as a URL reference.
    crate::codec::ir::IrImageSource::Url(url.to_string())
}

/// Signal that this egress dialect cannot express [`crate::codec::ir::IrTool::strict`], naming the
/// tools whose strict-schema guarantee is dropped. `strict: true` is a behavioural contract (OpenAI
/// guarantees schema-conforming arguments, so callers skip validation). Gemini has no strict flag
/// and Cohere's request-level `strict_tools` cannot express disagreeing tools, so it cannot cross,
/// but it crossed silently. One warn per request naming the tools makes the drop visible.
///
/// Called by the writers that cannot express it (Gemini; Cohere for disagreeing tools); those that
/// can (OpenAI Chat, Responses, Anthropic, Bedrock) emit it and never call this.
pub fn warn_dropped_tool_strict(tools: &[crate::codec::ir::IrTool], egress: &'static str) {
    let named: Vec<&str> = tools
        .iter()
        .filter(|t| t.strict.is_some())
        .map(|t| t.name.as_str())
        .collect();
    if named.is_empty() {
        return;
    }
    crate::codec::drops::writer_drop!(
        crate::codec::drops::STRICT,
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        [egress = %egress, tools = %named.join(","),],
        "dropping `tools[].strict` on this egress: the target protocol has no per-tool strict-schema \
         flag, so the model's arguments are NO LONGER GUARANTEED to conform to the tool schema. \
         Validate tool arguments on this route, or pin the request to an openai/responses lane"
    );
}

/// The minimal one-token "ping" IR request a health probe serializes through a dialect's own
/// `write_request`, so no per-protocol probe code is needed. Shared by every writer's
/// `probe_request`; the caller stamps the model via `rewrite_model_if_needed`.
pub fn ping_request() -> crate::codec::ir::IrRequest {
    use crate::codec::ir::{IrBlock, IrMessage, IrRequest, IrRole};
    // Only the fields a ping sets are named; the rest is the IR default, so a new IR slot never
    // needs threading through here or changes the probe bytes.
    IrRequest {
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![IrBlock::Text {
                text: "ping".to_string(),
                cache_control: None,
                citations: vec![],
                refusal: false,
            }],
        }],
        max_tokens: Some(1),
        ..IrRequest::default()
    }
}

/// Synthesize a deterministic non-empty `call_…` tool-call id for a response tool call whose
/// upstream `id` was blank or absent (a blank `tool_use.id` is rejected by the Anthropic reader and
/// breaks `tool_result` correlation). Derived from `(ordinal, name)` via the stdlib fixed-seed
/// `DefaultHasher` (stable within a run) so repeated names stay distinct; the `call_` prefix is
/// OpenAI's shape and visibly synthetic. Mirrors the gemini reader's `synth_tool_call_id`.
pub(crate) fn synth_response_tool_call_id(ordinal: usize, name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ordinal.hash(&mut hasher);
    name.hash(&mut hasher);
    format!("call_{:016x}", hasher.finish())
}

/// Decode a tool-call `arguments` field into the IR tool input. OpenAI's shape is a JSON string
/// (parsed; a malformed string is kept verbatim as a `String` so nothing is lost). Some compatible
/// backends send an already-parsed object, which `as_str().unwrap_or("{}")` collapsed to `{}`,
/// discarding the arguments; a non-string value is used directly. Absent gives `{}` (a no-arg call).
pub(crate) fn tool_input_from_arguments(v: Option<&serde_json::Value>) -> serde_json::Value {
    match v {
        Some(serde_json::Value::String(s)) => crate::codec::json::parse_str(s)
            .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
        Some(other) => other.clone(),
        None => serde_json::json!({}),
    }
}
