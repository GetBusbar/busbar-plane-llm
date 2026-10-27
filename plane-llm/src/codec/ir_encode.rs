// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR ↔ wire projection helpers shared across the LLM dialect codecs.
//!
//! Small pure projections between concrete IR and wire-shape decisions: the image-source pair
//! (`image_url` string ↔ IR, both directions), structured-json tool-result detection, and the
//! `tools[].strict` drop warning. They name concrete IR, so they belong beside the codecs that use
//! them rather than in the neutral router; every caller is a `busbar-llm` reader/writer reached
//! through the vtable, so core never names them.

/// Reconstruct an OpenAI/Responses `image_url` string from an [`crate::codec::ir::IrImageSource`] —
/// the inverse of core's `parse_image_url`. A `Url` is emitted verbatim; a `Base64` is re-wrapped
/// into a `data:<mime>;base64,<payload>` URI. A `Vendor` reference has no `image_url` projection, so
/// returns `None` and the caller drops the block with a warn. Shared by the Chat and Responses writers.
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

/// Decompose an OpenAI/Responses `image_url` string into the IR image source — the inverse of
/// [`image_url_from_ir`]. Shared verbatim by the Chat and Responses readers (both surfaces use the
/// same `image_url` wire shape).
///
/// A `data:<mime>;base64,<payload>` URI is decomposed into its real MIME type ("image/png") and raw
/// base64 payload, matching the IR contract the Anthropic reader/writer use for base64 images. Any
/// other URL (an https reference, or a data URI we cannot confidently split) is preserved verbatim in
/// `data` with an "image_url" media_type sentinel so the writer can reconstruct the exact original
/// `image_url` on a same-protocol round-trip rather than mangling it.
pub fn parse_image_url(url: &str) -> crate::codec::ir::IrImageSource {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, payload)) = rest.split_once(',') {
            // meta is e.g. "image/png;base64" or "image/png" — keep only the MIME type.
            let media_type = meta.split(';').next().unwrap_or("").to_string();
            if meta.contains("base64") && !media_type.is_empty() {
                return crate::codec::ir::IrImageSource::Base64 {
                    media_type,
                    data: payload.to_string(),
                };
            }
        }
    }
    // Non-data URL (https://...) or an unrecognized data URI: keep it verbatim as a URL reference so
    // the writer round-trips it as-is rather than mangling it.
    crate::codec::ir::IrImageSource::Url(url.to_string())
}

/// Signal that this egress dialect cannot express [`crate::codec::ir::IrTool::strict`], naming the
/// tools whose strict-schema guarantee is being dropped.
///
/// `strict: true` is a BEHAVIOURAL contract — OpenAI guarantees the model's tool arguments conform
/// to the schema exactly, and callers legitimately skip validation because of it. Gemini has no
/// strict flag, and Cohere's request-level `strict_tools` cannot express tools that disagree, so
/// there the guarantee genuinely cannot cross — but it was crossing SILENTLY, which turned "your
/// tool arguments are schema-guaranteed" into "they are not" with nothing said. One warn per request
/// naming the affected tools, so the drop is at least visible in the logs.
///
/// Called by the writers that cannot express the flag (Gemini; Cohere for disagreeing tools); the
/// writers that CAN (OpenAI Chat, Responses, Anthropic, Bedrock) emit it instead and never call this.
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

/// The minimal one-token "ping" IR request an active health probe serializes through a dialect's own
/// `write_request` — so every protocol gets a valid probe body for free, no per-protocol probe code.
/// Shared by every writer's `probe_request` seam; the model is stamped afterward by the caller's
/// `rewrite_model_if_needed`, so this carries none.
pub fn ping_request() -> crate::codec::ir::IrRequest {
    use crate::codec::ir::{IrBlock, IrMessage, IrRequest, IrRole};
    // Only the fields a ping sets are named; everything else is the IR default (absent), so a new
    // IR slot never has to be threaded through here and never changes the probe bytes.
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

/// Synthesize a deterministic, non-empty `call_…` tool-call id for a RESPONSE tool call whose
/// upstream body carried a blank/absent `id`. A blank id becomes an empty `tool_use.id`/`tool_call_id`
/// that the cross-protocol target reader (Anthropic) rejects and that breaks `tool`/`tool_result`
/// correlation. Derived from `(ordinal, name)` via the stdlib fixed-seed `DefaultHasher` (SipHash-1-3,
/// no new dependency, stable within a run) so repeated function names in one response stay distinct;
/// the `call_` prefix matches OpenAI's native tool-id shape and keeps it visibly synthetic. Mirrors
/// the gemini reader's `synth_tool_call_id` discipline for the same never-blank-correlation-key reason.
pub(crate) fn synth_response_tool_call_id(ordinal: usize, name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ordinal.hash(&mut hasher);
    name.hash(&mut hasher);
    format!("call_{:016x}", hasher.finish())
}

/// Decode a tool-call `arguments` field into the IR tool-input `Value`. OpenAI's native shape is a
/// JSON STRING (parsed to a value; a malformed string is preserved verbatim as a `String` rather than
/// dropped, so no arguments are lost). But some OpenAI-compatible backends emit `arguments` as an
/// already-parsed JSON OBJECT (or other non-string value); the prior `as_str().unwrap_or("{}")`
/// COLLAPSED that to an empty object, silently discarding the caller's tool arguments. Use a non-string
/// value directly instead. Absent `arguments` yields `{}` (a no-arg call).
pub(crate) fn tool_input_from_arguments(v: Option<&serde_json::Value>) -> serde_json::Value {
    match v {
        Some(serde_json::Value::String(s)) => crate::codec::json::parse_str(s)
            .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
        Some(other) => other.clone(),
        None => serde_json::json!({}),
    }
}
