// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Cohere v2 protocol reader/writer implementation.

use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use crate::codec::protocol::*;
use busbar_contract::http::StatusCode;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
#[cfg(test)]
use busbar_contract::upstream::StatusClass;

#[allow(unused_imports)]
use super::proto_codec::*;

#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

pub mod handler;
#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
mod reader;
mod writer;

/// Build this dialect's wire codec. A fresh instance per resolution (the writer carries per-stream
/// mutable state).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, CohereReader, CohereWriter)
}

/// Router detection: the v2/v1 chat paths (`/v2/chat`, `/v1/chat`, rung 8) and the v2 embed/rerank
/// paths (`/v2/embed`, `/v2/rerank`, rung 9).
fn claims(
    _h: crate::codec::protocol::HeadFields<'_>,
    path: &str,
) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::protocol::ClaimStrength;
    if path.ends_with("/v2/chat") || path.ends_with("/v1/chat") {
        return Some(ClaimStrength(8));
    }
    if path.ends_with("/v2/embed") || path.ends_with("/v2/rerank") {
        return Some(ClaimStrength(9));
    }
    None
}

/// Headerless residual detection: an exact `/v2/chat` names Cohere (rung 50).
fn residual_claims(path: &str) -> Option<crate::codec::protocol::ClaimStrength> {
    if path == "/v2/chat" {
        return Some(crate::codec::protocol::ClaimStrength(50));
    }
    None
}

/// Cohere's declaration.
pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::CohereRequestHandler),
    verbs: &[
        busbar_contract::operation::OpVerb::CHAT,
        busbar_contract::operation::OpVerb::EMBEDDINGS,
        busbar_contract::operation::OpVerb::RERANK,
    ],
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    streaming_content_type: Some(crate::codec::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: None,

    native_tool_id_prefix: None,
    ingress_auth: IngressAuth::Bearer,

    egress_auth_headers: None,
    egress_auth_lane_constant: false,
    egress_scheme: Some(EgressScheme::bearer()),
    stream_usage_requires_opt_in: false,
    requires_max_tokens: false,
    stop_sequence_cap: Some((5, "Cohere")),
    cache_markers_model_gated: false,
    fills_thought_signature: false,
    frame_after_message_start: None,
    reshapes_body_at_path_base: false,
    max_cache_control_breakpoints: None,
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: false,

    max_citations_per_delta: Some(1),

    egress_user_agent: "cohere-python/5.11.0",
    has_model_in_url: false,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::UNAUTHORIZED.as_u16(),
        crate::codec::protocol::ERR_TYPE_AUTHENTICATION,
    ),
    ingress_relays_amzn_headers: false,
    ingress_relayed_response_header_names: &[],
    auth_failure_message: "invalid api token",
    uses_array_stream_shim: false,
    has_native_path_not_found: false,
    egress_stream_accept: crate::codec::protocol::TEXT_EVENT_STREAM,

    models_list_envelope: None,
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: None,

    list_models_fingerprint_headers: &[],
    static_headers: &[],
};

/// This dialect's registration (its one line is in `crate::codec::DIALECTS`).
pub(crate) const ENTRY: super::proto_codec::DialectEntry = super::proto_codec::DialectEntry {
    decl: &DECL,
    protocol,
    with_writer: |f| {
        let w = CohereWriter;
        f(&w)
    },
    with_reader: |f| f(&CohereReader),
    leaf: &handler::LEAF,
};

/// Upstream URL path for the Cohere v2 chat endpoint (mirrors `PATH_UPSTREAM` in openai_chat.rs and anthropic.rs).
const PATH_UPSTREAM: &str = "/v2/chat";

/// Read a Cohere v2 `message.citations[]` array into neutral IR citations. Cohere spells one
/// `{"start", "end", "text", "type": "TEXT_CONTENT", "sources": [{"type": "document", "id",
/// "document"}]}`.
pub fn read_cohere_citations(citations: &serde_json::Value) -> Vec<crate::codec::ir::IrCitation> {
    let mut out = Vec::new();
    let Some(arr) = citations.as_array() else {
        return out;
    };
    for entry in arr {
        let start = entry.get(keys::START).and_then(|v| v.as_i64());
        let end = entry.get(keys::END).and_then(|v| v.as_i64());
        let cited_text = entry
            .get(keys::TEXT)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        if start.is_none() && end.is_none() && cited_text.is_none() {
            continue;
        }

        let first_source = entry
            .get(SOURCES)
            .and_then(|s| s.as_array())
            .and_then(|a| a.first());
        let doc = first_source.and_then(|s| s.get(keys::DOCUMENT));
        out.push(crate::codec::ir::IrCitation {
            domain: None,

            kind: Some("web_search_result_location".to_string()),
            cited_text,
            title: doc
                .and_then(|d| d.get(keys::TITLE))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            url: doc
                .and_then(|d| d.get(keys::URL))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            document_index: None,
            start_index: start,
            end_index: end,
            encrypted_index: None,
            raw: Some(entry.clone()),
            ..Default::default()
        });
    }
    out
}

/// Precomputed Unicode boundaries make both block selection and clipped text extraction
/// independent of the amount of answer text preceding a citation.
struct CohereCitationRange {
    block: usize,
    base: i64,
    end: i64,
    byte_offsets: Vec<usize>,
}

fn cohere_citation_ranges(blocks: &[crate::codec::ir::IrBlock]) -> Vec<CohereCitationRange> {
    let mut base = 0i64;
    blocks
        .iter()
        .enumerate()
        .filter_map(|(block, value)| {
            let crate::codec::ir::IrBlock::Text { text, .. } = value else {
                return None;
            };
            let mut byte_offsets: Vec<_> = text.char_indices().map(|(offset, _)| offset).collect();
            let end = base.saturating_add(byte_offsets.len() as i64);
            byte_offsets.push(text.len());
            let range = CohereCitationRange {
                block,
                base,
                end,
                byte_offsets,
            };
            base = end;
            Some(range)
        })
        .collect()
}

fn cohere_citation_overlap(
    ranges: &[CohereCitationRange],
    start: i64,
    stop: i64,
) -> &[CohereCitationRange] {
    let first = ranges.partition_point(|range| range.end <= start);
    let count = ranges[first..].partition_point(|range| range.base < stop);
    &ranges[first..first + count]
}

#[derive(Default)]
struct CohereCitationBudget {
    bytes: usize,
    items: usize,
}

impl CohereCitationBudget {
    fn charge(&mut self, bytes: usize, items: usize) -> Result<(), IrError> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.items = self.items.saturating_add(items);
        if self.bytes > crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT || self.items > 65536
        {
            return Err(crate::codec::dialect::ir_parse_error());
        }
        Ok(())
    }
}

// Count owned allocations conservatively once, without serializing or copying the source.
fn cohere_citation_cost(citation: &crate::codec::ir::IrCitation) -> Result<usize, IrError> {
    fn value_cost(
        value: &serde_json::Value,
        budget: &mut CohereCitationBudget,
        depth: usize,
    ) -> Result<(), IrError> {
        if depth > 128 {
            return Err(crate::codec::dialect::ir_parse_error());
        }
        budget.charge(std::mem::size_of::<serde_json::Value>(), 0)?;
        match value {
            serde_json::Value::String(text) => budget.charge(text.len(), 0)?,
            serde_json::Value::Array(values) => {
                for value in values {
                    value_cost(value, budget, depth + 1)?;
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    budget.charge(key.len().saturating_add(128), 0)?;
                    value_cost(value, budget, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut budget = CohereCitationBudget::default();
    budget.charge(std::mem::size_of::<crate::codec::ir::IrCitation>(), 0)?;
    for text in [
        &citation.kind,
        &citation.cited_text,
        &citation.title,
        &citation.url,
        &citation.encrypted_index,
        &citation.domain,
    ]
    .into_iter()
    .flatten()
    {
        budget.charge(text.len(), 0)?;
    }
    if let Some(file) = &citation.file {
        for text in [&file.file_id, &file.filename].into_iter().flatten() {
            budget.charge(text.len(), 0)?;
        }
    }
    if let Some(raw) = &citation.raw {
        value_cost(raw, &mut budget, 0)?;
    }
    Ok(budget.bytes)
}

/// Distribute message-global character spans across text blocks. Returns whether a
/// citation-only text block was added (so the caller can supply its origin).
fn attach_cohere_citations(
    blocks: &mut Vec<crate::codec::ir::IrBlock>,
    citations: Vec<crate::codec::ir::IrCitation>,
    budget: &mut CohereCitationBudget,
) -> Result<bool, IrError> {
    use crate::codec::ir::IrBlock;
    if citations.is_empty() {
        return Ok(false);
    }
    let ranges = cohere_citation_ranges(blocks);
    let Some(first) = ranges.first() else {
        for citation in &citations {
            budget.charge(cohere_citation_cost(citation)?, 1)?;
        }
        blocks.push(IrBlock::Text {
            text: String::new(),
            cache_control: None,
            citations,
            refusal: false,
        });
        return Ok(true);
    };
    for citation in citations {
        let span = citation
            .start_index
            .zip(citation.end_index)
            .filter(|(start, end)| *start >= 0 && end >= start);
        let overlaps = span
            .map(|(start, stop)| cohere_citation_overlap(&ranges, start, stop))
            .unwrap_or(&[]);
        let copies = overlaps.len().max(1);
        budget.charge(
            cohere_citation_cost(&citation)?.saturating_mul(copies),
            copies,
        )?;
        // Account for replacement quoted text before cloning any citation metadata.
        if let Some((start, stop)) = span {
            for range in overlaps {
                if start < range.base || stop > range.end {
                    let left = (start.max(range.base) - range.base) as usize;
                    let right = (stop.min(range.end) - range.base) as usize;
                    budget.charge(range.byte_offsets[right] - range.byte_offsets[left], 0)?;
                }
            }
        }
        let mut attached = false;
        if let Some((start, stop)) = span {
            for range in cohere_citation_overlap(&ranges, start, stop) {
                let IrBlock::Text {
                    text, citations, ..
                } = &mut blocks[range.block]
                else {
                    unreachable!()
                };
                let local_start = start.max(range.base) - range.base;
                let local_end = stop.min(range.end) - range.base;
                let mut local = citation.clone();
                local.start_index = Some(local_start);
                local.end_index = Some(local_end);
                if start < range.base || stop > range.end {
                    local.cited_text = Some(
                        text[range.byte_offsets[local_start as usize]
                            ..range.byte_offsets[local_end as usize]]
                            .to_string(),
                    );
                }
                citations.push(local);
                attached = true;
            }
        }
        if !attached {
            if let IrBlock::Text { citations, .. } = &mut blocks[first.block] {
                citations.push(citation);
            }
        }
    }
    Ok(false)
}

/// Project a neutral [`crate::codec::ir::IrCitation`] into Cohere v2's citation object, the inverse
/// of [`read_cohere_citations`].
fn write_cohere_citation(
    c: &crate::codec::ir::IrCitation,
    span: Option<(i64, i64)>,
) -> Option<serde_json::Value> {
    if let Some(raw) = &c.raw {
        if raw.get(keys::START).is_some() || raw.get(SOURCES).is_some() {
            let mut raw = raw.clone();
            if let (Some(object), Some((start, end))) = (raw.as_object_mut(), span) {
                object.insert(keys::START.to_string(), serde_json::json!(start));
                object.insert(keys::END.to_string(), serde_json::json!(end));
                if let Some(text) = &c.cited_text {
                    object.insert(keys::TEXT.to_string(), serde_json::json!(text));
                }
            }
            return Some(raw);
        }
    }
    let mut obj = serde_json::Map::new();
    if let Some((start, end)) = span {
        obj.insert(keys::START.to_string(), serde_json::json!(start));
        obj.insert(keys::END.to_string(), serde_json::json!(end));
    } else if c.start_index.is_some() || c.end_index.is_some() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::member("citations"),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [],
            "dropping unanchored citation coordinates on Cohere egress"
        );
    }
    if let Some(t) = &c.cited_text {
        obj.insert(keys::TEXT.to_string(), serde_json::json!(t));
    }
    let mut source = serde_json::Map::new();
    if let Some(u) = &c.url {
        source.insert(keys::URL.to_string(), serde_json::json!(u));
    }
    if let Some(t) = &c.title {
        source.insert(keys::TITLE.to_string(), serde_json::json!(t));
    }
    if !source.is_empty() {
        obj.insert(
            SOURCES.to_string(),
            serde_json::json!([{ (keys::TYPE): keys::DOCUMENT, (keys::DOCUMENT): serde_json::Value::Object(source) }]),
        );
    }
    if obj.is_empty() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::member("citations"),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [],
            "dropping citation with no Cohere representation"
        );
        None
    } else {
        Some(serde_json::Value::Object(obj))
    }
}

/// Cohere citations address the concatenated answer, while IR offsets are local to a text block.
fn write_cohere_citations(blocks: &[crate::codec::ir::IrBlock]) -> Vec<serde_json::Value> {
    let mut base = 0usize;
    let mut out = Vec::new();
    for block in blocks {
        if let crate::codec::ir::IrBlock::Text {
            text, citations, ..
        } = block
        {
            out.extend(citations.iter().filter_map(|citation| {
                write_cohere_citation(
                    citation,
                    crate::codec::url_citation_wire::citation_span(text, base, citation),
                )
            }));
            base = base.saturating_add(text.chars().count());
        }
    }
    out
}

/// A projected citation must carry some meaning in the target wire; never substitute `{}` for
/// a foreign citation whose fields this writer cannot represent.
fn citation_representable(citation: &crate::codec::ir::IrCitation) -> bool {
    citation
        .raw
        .as_ref()
        .is_some_and(|raw| raw.get(keys::START).is_some() || raw.get(SOURCES).is_some())
        || (matches!(
            citation.kind.as_deref(),
            None | Some("web_search_result_location")
        ) && citation
            .start_index
            .zip(citation.end_index)
            .is_some_and(|(s, e)| s >= 0 && e >= s))
        || [&citation.cited_text, &citation.url, &citation.title]
            .iter()
            .any(|value| value.as_ref().is_some_and(|value| !value.is_empty()))
}

fn response_projection_error() -> IrError {
    IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("Cohere response loses tool or citation meaning".into()),
        retry_after: None,
    }
}

/// The `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`] this protocol produces: a Cohere
/// v2 tool-result `document`, whose `data` is an arbitrary string map, not bytes with a mime type,
/// so it has no neutral base64/url form.
const VENDOR_NAME: &str = "cohere";

const BINARY: &str = "binary";

const CLASSIFICATIONS: &str = "classifications";

const EMBEDDINGS: &str = "embeddings";

const EMBEDDING_TYPES: &str = "embedding_types";

const FLOAT: &str = "float";

const INPUT_TYPE: &str = "input_type";

const INT8: &str = "int8";

const MAX_TOKENS_PER_DOC: &str = "max_tokens_per_doc";

const OUTPUT_DIMENSION: &str = "output_dimension";

const SOURCES: &str = "sources";

const STRICT_TOOLS: &str = "strict_tools";

const SOURCE_GROUNDING_DOCUMENT: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "documents[]",
    };
const SOURCE_MESSAGE_CONTENT: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content",
};
const SOURCE_MESSAGE_TEXT: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=text",
};
const SOURCE_MESSAGE_IMAGE: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=image_url",
};
const SOURCE_MESSAGE_DOCUMENT: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=document",
};
const SOURCE_MESSAGE_THINKING: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].type=thinking",
};
const SOURCE_MESSAGE_TOOL_PLAN: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].tool_plan",
};
const SOURCE_MESSAGE_CITATIONS: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].citations",
};
const SOURCE_MESSAGE_TOOL_CALL: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].tool_calls[]",
};
const SOURCE_RESPONSE_TOOL_PLAN: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "message.tool_plan",
    };
const SOURCE_RESPONSE_TEXT: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "message.content[].type=text",
};
const SOURCE_RESPONSE_THINKING: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "message.content[].type=thinking",
};
const SOURCE_RESPONSE_CITATIONS: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "message.citations",
    };
const SOURCE_RESPONSE_TOOL_CALL: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "message.tool_calls[]",
    };

const SOURCE_STREAM_THINKING_START: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "content-start.delta.message.content",
    };
const SOURCE_STREAM_THINKING_DELTA: crate::codec::ir::IrBlockSource =
    crate::codec::ir::IrBlockSource {
        dialect: VENDOR_NAME,
        path: "content-delta.delta.message.content",
    };
const SOURCE_STREAM_TEXT_START: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "content-start",
};
const SOURCE_STREAM_TEXT_DELTA: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "content-delta",
};
const SOURCE_STREAM_TOOL_PLAN: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "tool-plan-delta",
};
const SOURCE_STREAM_TOOL_CALL: crate::codec::ir::IrBlockSource = crate::codec::ir::IrBlockSource {
    dialect: VENDOR_NAME,
    path: "tool-call-start",
};
/// The Cohere v2 request content grammar (`codec::drops`). A part of any other kind is named as
/// dropped on a translate attempt.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["messages[]", "content[]"],
    tag: Some(keys::TYPE),
    modelled: &[keys::TEXT, keys::IMAGE_URL, keys::DOCUMENT, keys::THINKING],
    companions: &[],
}];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, "type=text"),
    (crate::codec::drops::kind::IMAGE, "type=image_url"),
    (crate::codec::drops::kind::DOCUMENT, "type=document"),
    (crate::codec::drops::kind::THINKING, "type=thinking"),
];

/// IR request members the reader carries by code from a path no map-file row names.
const REQUEST_CODE_NAMES: &[(&str, &str)] = &[
    (crate::codec::drops::name::REASONING, keys::THINKING),
    (
        crate::codec::drops::name::THINKING_BUDGET,
        "thinking.token_budget",
    ),
];

const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["message", "content[]"],
    tag: Some(keys::TYPE),
    modelled: &[keys::TEXT, keys::THINKING],
    companions: &[],
}];

/// What this dialect's answers carry beyond its map file's rows (drop walk, design F3 "Drops").
const RESPONSE_CODE: &[&str] = &["usage.cached_tokens", "model"];
const STREAM_CODE: &[&str] = &[];

/// Answer paths inside a carried subtree that its code does not carry (where a coarse map row covers them).
const RESPONSE_DROPS: &[&str] = &[
    "message.citations[].content_index",
    "message.citations[].sources[].type=tool.tool_output",
];
const STREAM_DROPS: &[&str] = &[
    "type=citation-start.delta.message.citations.content_index",
    "type=citation-start.delta.message.citations.sources[].type=tool.tool_output",
];

const TEXTS: &str = "texts";

const TOKEN_BUDGET: &str = "token_budget";

const TOOL_PLAN: &str = "tool_plan";

const TRUNCATE: &str = "truncate";

const UBINARY: &str = "ubinary";

const UINT8: &str = "uint8";

/// Cap on distinct tool-call frame indices in `state.open_tools` for one stream. The set is never
/// shrunk (each tool's IR block index stays stable, see `state.tool_ir_index`), so an upstream
/// streaming unbounded distinct `tool-call-start` indices would grow it without bound.
const MAX_TRACKED_TOOL_FRAMES: usize = 4096;

/// Sentinel recorded in `state.open_tools` the first time a text block opens, encoding that "a text
/// block has occupied IR index 0 this stream" so tool blocks stay off index 0 even after the text
/// block closes (`text_block_open` reverts on `content-end`).
const TEXT_BLOCK_SEEN_SENTINEL: usize = usize::MAX;

/// Bound on the upstream-controlled stream-frame `index` at every tool-call read site, matching
/// `MAX_TRACKED_TOOL_FRAMES`: it keeps every recorded `frame_idx` far below
/// `TEXT_BLOCK_SEEN_SENTINEL` and leaves realistic streams (small sequential indices) alone.
const MAX_TOOL_FRAME_INDEX: u64 = MAX_TRACKED_TOOL_FRAMES as u64;

const ET_MESSAGE_START: &str = "message-start";
const ET_MESSAGE_END: &str = "message-end";
const ET_CONTENT_START: &str = "content-start";
const ET_CONTENT_DELTA: &str = "content-delta";
/// Stream `type` of the tool-plan-delta event, the streamed `message.tool_plan` (the assistant's
/// pre-tool-call reasoning): one token per frame ahead of `tool-call-start`, text at
/// `delta.message.tool_plan`.
const ET_TOOL_PLAN_DELTA: &str = "tool-plan-delta";

/// Cohere v2's streamed-citation frame. The writer once suppressed streamed citations on the belief
/// there was no such frame, so a grounded answer had sources non-streaming but not streaming.
const ET_CITATION_START: &str = "citation-start";
/// Stream `type` of the citation-end event, the structural close that pairs with each
/// `citation-start` (docs.cohere.com/v2/docs/streaming).
const ET_CITATION_END: &str = "citation-end";
const ET_CONTENT_END: &str = "content-end";
const ET_TOOL_CALL_START: &str = "tool-call-start";
const ET_TOOL_CALL_DELTA: &str = "tool-call-delta";
const ET_TOOL_CALL_END: &str = "tool-call-end";

/// Cohere v2 `finish_reason` for a normal end-of-turn completion.
const COHERE_FINISH_COMPLETE: &str = "COMPLETE";
/// Cohere v2 `finish_reason` for a content-moderation stop.
const COHERE_FINISH_ERROR_TOXIC: &str = "ERROR_TOXIC";
/// Cohere v2 `finish_reason` for an infrastructure/generic error stop.
const COHERE_FINISH_ERROR: &str = "ERROR";
/// Cohere v2 `finish_reason` for a stop-sequence stop.
const COHERE_FINISH_STOP_SEQUENCE: &str = "STOP_SEQUENCE";
/// Cohere v2 `finish_reason` for a tool-call stop.
const COHERE_FINISH_TOOL_CALL: &str = "TOOL_CALL";
/// Cohere v2 `finish_reason` for a max-tokens stop.
const COHERE_FINISH_MAX_TOKENS: &str = "MAX_TOKENS";

/// Cohere v2 `tool_choice` value requiring at least one tool call.
const COHERE_TOOL_CHOICE_REQUIRED: &str = "REQUIRED";
/// Cohere v2 `tool_choice` value forbidding all tool calls.
const COHERE_TOOL_CHOICE_NONE: &str = "NONE";

/// Preserve absent-index compatibility, but reject malformed or out-of-range identities
/// rather than aliasing them to another tool call.
fn cohere_frame_index(data: &serde_json::Value) -> Result<usize, IrError> {
    let Some(value) = data.get(keys::INDEX) else {
        return Ok(0);
    };
    value
        .as_u64()
        .filter(|index| *index <= MAX_TOOL_FRAME_INDEX)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(crate::codec::dialect::ir_parse_error)
}

/// Normalize Cohere v2's `tool_choice` (a top-level enum string) into the IR union so a forced
/// directive survives the seam instead of degrading to `auto`.
fn read_cohere_tool_choice(
    val: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    match val?.as_str()? {
        COHERE_TOOL_CHOICE_REQUIRED => Some(crate::codec::ir::IrToolChoice::Required),
        COHERE_TOOL_CHOICE_NONE => Some(crate::codec::ir::IrToolChoice::None),
        _ => None,
    }
}

/// Read a Cohere v2 `response_format` into [`crate::codec::ir::IrResponseFormat`]. The only code that
/// knows its shape: `{"type":"text"}`, `{"type":"json_object"}`, or `{"type":"json_object","json_schema":<schema>}`
/// (the schema sits directly under `json_schema`, not under `.schema` as in OpenAI).
fn read_cohere_response_format(
    v: &serde_json::Value,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let o = v.as_object()?;
    match o.get(keys::TYPE).and_then(|t| t.as_str()) {
        Some(keys::TEXT) => Some(crate::codec::ir::IrResponseFormat::bare(false, None)),

        Some(_) => Some(crate::codec::ir::IrResponseFormat::bare(
            true,
            o.get(keys::JSON_SCHEMA)
                .filter(|s| s.is_object())
                .cloned()
                .or_else(|| o.get("schema").cloned()),
        )),
        None => None,
    }
}

/// Project [`crate::codec::ir::IrResponseFormat`] into Cohere v2's `response_format`; the only code that builds it.
fn write_cohere_response_format(rf: &crate::codec::ir::IrResponseFormat) -> serde_json::Value {
    if !rf.json {
        return serde_json::json!({ (keys::TYPE): keys::TEXT });
    }
    match &rf.schema {
        Some(schema) => {
            serde_json::json!({ (keys::TYPE): keys::JSON_OBJECT, (keys::JSON_SCHEMA): schema })
        }
        None => serde_json::json!({ (keys::TYPE): keys::JSON_OBJECT }),
    }
}

/// Read Cohere v2's request `thinking` into the IR reasoning ask (see `read_request`):
fn read_cohere_reasoning(
    v: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrReasoningAsk> {
    let t = v?.as_object()?;
    match t.get(keys::TYPE).and_then(|ty| ty.as_str()) {
        Some(keys::ENABLED) => {}

        Some(keys::DISABLED) => return Some(crate::codec::ir::IrReasoningAsk::Off),
        _ => return None,
    }
    match t.get(TOKEN_BUDGET) {
        None | Some(serde_json::Value::Null) => Some(crate::codec::ir::IrReasoningAsk::Dynamic),
        Some(b) => b
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(crate::codec::ir::IrReasoningAsk::Budget),
    }
}

/// Project the IR reasoning ask into Cohere v2's `thinking`, inverse of [`read_cohere_reasoning`]. A
/// numeric budget is emitted as is and an effort word goes through the operator's effort-budget
/// table; `Dynamic` is Cohere's own budget-less `{type:"enabled"}`, needing no table guess.
fn write_cohere_reasoning(
    ask: crate::codec::ir::IrReasoningAsk,
    table: [u32; 4],
) -> serde_json::Value {
    match ask {
        crate::codec::ir::IrReasoningAsk::Off => {
            serde_json::json!({ (keys::TYPE): keys::DISABLED })
        }

        other => match other.to_budget(table) {
            Some(budget) => {
                serde_json::json!({ (keys::TYPE): keys::ENABLED, (TOKEN_BUDGET): budget })
            }
            None => serde_json::json!({ (keys::TYPE): keys::ENABLED }),
        },
    }
}

/// Read a Cohere v2 document (`{"id"?: "…", "data": {…}}`: a user content part's `document`, or an
/// entry of top-level `documents`) into a document [`crate::codec::ir::IrBlock::Media`] (COH-04,
/// IR-13).
fn read_cohere_document(doc: &serde_json::Value) -> crate::codec::ir::IrBlock {
    let text_document = |text: &str, name: Option<&str>| crate::codec::ir::IrBlock::Media {
        kind: crate::codec::ir::IrMediaKind::Document,
        source: crate::codec::ir::IrImageSource::Base64 {
            media_type: keys::TEXT_PLAIN.to_string(),
            data: busbar_contract::media::base64_encode(text.as_bytes()),
        },
        name: name.filter(|s| !s.is_empty()).map(String::from),
        cache_control: None,
        citations: None,
        context: None,
    };
    if let Some(text) = doc.as_str() {
        return text_document(text, None);
    }
    let id = doc
        .get(keys::ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match doc.get(keys::DATA) {
        Some(serde_json::Value::String(text)) => text_document(text, id),
        Some(serde_json::Value::Object(d)) => {
            let title = d
                .get(keys::TITLE)
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty());
            let name = title.or(id);
            let plain = d.iter().all(|(k, v)| match k.as_str() {
                keys::TEXT | keys::TITLE => v.is_string(),
                _ => false,
            });
            match d.get(keys::TEXT).and_then(|t| t.as_str()) {
                Some(text) if plain => text_document(text, name),
                _ => text_document(
                    &crate::codec::json::to_string(&serde_json::Value::Object(d.clone()))
                        .unwrap_or_default(),
                    name,
                ),
            }
        }
        _ => crate::codec::ir::IrBlock::Media {
            kind: crate::codec::ir::IrMediaKind::Document,
            source: crate::codec::ir::IrImageSource::Vendor {
                vendor: VENDOR_NAME,
                value: doc.clone(),
            },
            name: id.map(String::from),
            cache_control: None,
            citations: None,
            context: None,
        },
    }
}

/// Read the `detail` of a Cohere v2 `image_url` object (`"auto"|"low"|"high"`) into the requested
/// image fidelity (IR-08).
fn read_cohere_image_detail(
    image_url: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrImageDetail> {
    let word = image_url?.get(keys::DETAIL)?.as_str()?;
    let detail = crate::codec::ir::IrImageDetail::parse(word);
    if detail.is_none() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::wire("messages[].content[].image_url.detail"),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [detail = %word,],
            "cohere: dropping an unknown image_url.detail word (not auto/low/high)"
        );
    }
    detail
}

/// Read a Cohere v2 tool-result `document` part (`{"id"?: "…", "data": …}`) into the IR (the ANT-17
/// follow-up).
fn read_cohere_tool_result_document(doc: &serde_json::Value) -> crate::codec::ir::IrBlock {
    match doc.get(keys::DATA) {
        Some(serde_json::Value::String(text)) => crate::codec::ir::IrBlock::text(text.clone()),
        Some(data) if !data.is_null() => crate::codec::ir::IrBlock::Json(data.clone()),
        _ => crate::codec::ir::IrBlock::Media {
            kind: crate::codec::ir::IrMediaKind::Document,
            source: crate::codec::ir::IrImageSource::Vendor {
                vendor: VENDOR_NAME,
                value: doc.clone(),
            },
            name: doc
                .get(keys::ID)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            cache_control: None,
            citations: None,
            context: None,
        },
    }
}

/// The document object Cohere's top-level `documents` carries for an IR document Media (COH-18):
fn write_cohere_document(
    source: &crate::codec::ir::IrImageSource,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    match source {
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            Some(value.clone())
        }
        crate::codec::ir::IrImageSource::Base64 { media_type, data }
            if media_type
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("text/")) =>
        {
            let bytes = busbar_contract::media::base64_decode(data)?;
            let text = std::str::from_utf8(&bytes).ok()?;
            let mut d = serde_json::Map::new();
            d.insert(keys::TEXT.to_string(), serde_json::json!(text));
            if let Some(n) = name.filter(|s| !s.is_empty()) {
                d.insert(keys::TITLE.to_string(), serde_json::json!(n));
            }
            Some(serde_json::json!({ (keys::DATA): serde_json::Value::Object(d) }))
        }
        _ => None,
    }
}

/// Read one Cohere v2 `LogprobItem` (`{"text", "token_ids", "logprobs"}`) into
/// [`crate::codec::ir::IrTokenLogprob`] (COH-13 / COH-14).
fn read_cohere_logprob(item: &serde_json::Value) -> Option<crate::codec::ir::IrTokenLogprob> {
    let text = item.get(keys::TEXT)?.as_str()?;
    let lps = item.get(keys::LOGPROBS)?.as_array()?;
    if lps.is_empty() {
        return None;
    }
    let logprob = lps.iter().map(|v| v.as_f64()).sum::<Option<f64>>()?;
    Some(crate::codec::ir::IrTokenLogprob {
        token: text.to_string(),
        logprob,
        bytes: None,
        bytes_null: false,
        top: Vec::new(),
    })
}

/// Cohere v2 `finish_reason` to canonical [`crate::codec::ir::IrStopReason`]; the only read-side
/// place that knows the vocabulary.
fn read_cohere_stop_reason(token: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match token {
        COHERE_FINISH_COMPLETE => S::EndTurn,
        COHERE_FINISH_MAX_TOKENS => S::MaxTokens,
        COHERE_FINISH_TOOL_CALL => S::ToolUse,
        COHERE_FINISH_STOP_SEQUENCE => S::StopSequence,

        COHERE_FINISH_ERROR_TOXIC => S::Safety,
        COHERE_FINISH_ERROR => S::Error,

        _ => S::Other,
    }
}

/// Map a canonical IR stop reason to a valid Cohere v2 `finish_reason`. `ERROR` is first-class
/// (reader `ERROR` to `S::Error`, writer back).
fn write_cohere_stop_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::EndTurn => COHERE_FINISH_COMPLETE,
        S::StopSequence => COHERE_FINISH_STOP_SEQUENCE,
        S::MaxTokens => COHERE_FINISH_MAX_TOKENS,
        S::ToolUse => COHERE_FINISH_TOOL_CALL,
        S::Safety | S::Error => COHERE_FINISH_ERROR,
        S::Refusal | S::PauseTurn | S::Other => COHERE_FINISH_COMPLETE,
    }
}

/// Cohere's `usage.tokens.input_tokens` for an IR usage: the whole prompt. The IR's `input_tokens`
fn cohere_prompt_tokens(usage: &crate::codec::ir::IrUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
}

/// Format 16 bytes as a UUID-shaped (8-4-4-4-12 lowercase hex) token. Native Cohere v2 chat ids are
/// bare RFC-4122 UUIDv4s with no prefix (version nibble `4` opening group 3, variant `10xx` opening
/// group 4), so a synthesized id must match to stay indistinguishable.
fn format_uuid_layout(bytes: &[u8; 16]) -> String {
    let s = ::hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// Synthesize a Cohere-shaped response id where the backend supplied none (cross-protocol). Native
/// ids are bare RFC-4122 UUIDv4s, so this emits a proper v4: all 128 bits from the host CSPRNG, the
/// version nibble forced to `4` and the variant bits to `10xx`, with no embedded timestamp (an
/// earlier `secs << 32` layout leaked the server clock).
fn synthesize_cohere_id(host: &dyn crate::codec::cells::CodecHost) -> Result<String, IrError> {
    let mut bytes = [0u8; 16];
    if !host.fill_entropy(&mut bytes) {
        return Err(entropy_error());
    }

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Ok(format_uuid_layout(&bytes))
}

/// Number of reserved tool, thinking, and subsequent text frames (excludes the
/// first text slot recorded by `TEXT_BLOCK_SEEN_SENTINEL`).
fn cohere_tracked_tool_count(state: &crate::codec::ir::StreamDecodeState) -> usize {
    state.open_tools.len() - usize::from(state.open_tools.contains(&TEXT_BLOCK_SEEN_SENTINEL))
}

/// Record a `tool-call-start` for wire `frame_idx`, ASSIGNING it a stable IR block index, and return
/// that index.
fn cohere_assign_tool_ir_index(
    state: &mut crate::codec::ir::StreamDecodeState,
    frame_idx: usize,
) -> Option<usize> {
    if state.tool_ir_index.get(&frame_idx).copied().is_some() {
        return None;
    }
    let tracked = cohere_tracked_tool_count(state);

    if tracked >= MAX_TRACKED_TOOL_FRAMES {
        return None;
    }
    let base = usize::from(state.open_tools.contains(&TEXT_BLOCK_SEEN_SENTINEL));
    let ir_index = base + tracked;
    state.open_tools.insert(frame_idx);
    state.tool_ir_index.insert(frame_idx, ir_index);
    Some(ir_index)
}

/// Resolve the current text block index, assigning the first text slot on first appearance.
/// An explicit start after closure assigns a fresh reserved slot before calling this helper.
fn cohere_text_ir_index(state: &mut crate::codec::ir::StreamDecodeState) -> usize {
    let ti = state
        .text_index
        .unwrap_or_else(|| cohere_tracked_tool_count(state));
    state.text_index = Some(ti);
    state.open_tools.insert(TEXT_BLOCK_SEEN_SENTINEL);
    ti
}

/// First key of the reserved range a streamed `thinking` CONTENT block is recorded under in
/// `state.open_tools` / `state.tool_ir_index`.
const THINKING_FRAME_BASE: usize = usize::MAX / 2;
const TEXT_FRAME_BASE: usize = THINKING_FRAME_BASE / 2;

/// The IR index of the `thinking` content block that is open right now, if one is. A thinking
/// content block is open when `thinking_block_open` is set WITHOUT `text_block_open`; the two set
/// together mean the leading `tool-plan-delta` block, which rides the text slot instead.
fn cohere_open_thinking_index(state: &crate::codec::ir::StreamDecodeState) -> Option<usize> {
    if state.thinking_block_open && !state.text_block_open {
        state
            .tool_ir_index
            .range(THINKING_FRAME_BASE..)
            .next_back()
            .map(|(_, ir)| *ir)
    } else {
        None
    }
}

/// Open a new `thinking` content block: claim its IR index and emit its `BlockStart`. `None` (and
/// nothing emitted) only when the per-stream frame cap is reached.
fn cohere_open_thinking_block(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
    source: &'static crate::codec::ir::IrBlockSource,
) -> Option<usize> {
    let seen = state.tool_ir_index.range(THINKING_FRAME_BASE..).count();
    let index = cohere_assign_tool_ir_index(state, THINKING_FRAME_BASE.saturating_add(seen))?;
    state.thinking_block_open = true;
    state
        .block_origins
        .insert(index, crate::codec::ir::IrBlockOrigin::at(source));
    out.push(IrStreamEvent::BlockStart {
        index,
        block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
        refusal: false,
    });
    Some(index)
}

/// Close the open `thinking` content block, if there is one.
fn cohere_close_thinking_block(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if let Some(index) = cohere_open_thinking_index(state) {
        state.thinking_block_open = false;
        out.push(IrStreamEvent::BlockStop { index });
    }
}

/// Close the text slot (the answer's text block, or the leading `tool-plan-delta` block that rides
/// it) at its assigned index. Late deltas remain suppressed; an explicit new content start
/// may reset the latch only after reserving a fresh index.
fn cohere_close_text_slot(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if state.text_block_open {
        state.text_block_open = false;
        state.text_block_closed = true;

        state.thinking_block_open = false;
        out.push(IrStreamEvent::BlockStop {
            index: state.text_index.unwrap_or(0),
        });
    }
}

/// The `thinking` text a Cohere content object carries, when the object IS a thinking part: its
/// `type` says so, or (a real Cohere `content-delta`, which carries no `type`) it has a `thinking`
/// member and no `text` member.
fn cohere_thinking_part(content: &serde_json::Value) -> Option<&str> {
    let obj = content.as_object()?;
    let ty = obj.get(keys::TYPE).and_then(|t| t.as_str());
    let is_thinking = ty == Some(keys::THINKING)
        || (ty.is_none() && obj.contains_key(keys::THINKING) && !obj.contains_key(keys::TEXT));
    is_thinking.then(|| {
        obj.get(keys::THINKING)
            .and_then(|t| t.as_str())
            .unwrap_or("")
    })
}

#[derive(Clone)]
pub struct CohereReader;

impl CohereReader {
    /// True when the upstream error body carries Cohere v2's oversized-request ("context length
    /// exceeded") phrasing.
    fn body_signals_context_length(body: &[u8]) -> bool {
        let lower = String::from_utf8_lossy(body).to_lowercase();
        lower.contains("too many tokens")
            || (lower.contains("too long")
                && (lower.contains(keys::TOKEN)
                    || lower.contains(keys::CONTEXT)
                    || lower.contains("input")))
            || lower.contains("token limit")
            || (lower.contains("exceeds") && lower.contains(keys::CONTEXT))
            || (lower.contains(keys::MAXIMUM) && lower.contains(keys::TOKEN))
    }
}

#[derive(Clone)]
pub struct CohereWriter {
    stream: StreamCell<CohereStream>,
}

/// A Cohere stream's per-stream state (see [`CohereWriter`]).
#[derive(Clone)]
struct CohereStream {
    block_indices: crate::codec::proto_codec::EmittedBlockIndices,
    /// IR block indices for which this writer emitted a `tool-call-start` frame. The IR
    /// `BlockStop` carries only the integer index (no block kind), but a native Cohere v2 stream
    /// closes a tool-call block with `tool-call-end` and a text-content block with `content-end`.
    /// Emitting `content-end` for ALL `BlockStop` events — as a prior revision did — closed a
    /// tool-call block with the text-content close event, so a native Cohere SDK that distinguishes
    /// content events from tool-call events by type mis-decoded the stream. Track
    /// the tool-call opens here so `BlockStop` emits `tool-call-end` for a tool index and
    /// `content-end` for a text (or any non-tool) index. Per-stream INSTANCE state, mirroring the
    /// Responses writer's `open_tool_indices`.
    open_tool_indices: std::collections::BTreeSet<usize>,
    /// Per-stream set of text-block indices that emitted a `content-start` frame, so the matching
    /// `BlockStop` emits `content-end` ONLY for a block that actually opened. Cross-protocol blocks
    /// that carry no opening frame (redacted thinking / Image — see the `BlockStart` arm, which maps them to
    /// `None`) are never recorded here, so their `BlockStop` emits NOTHING rather than an orphan
    /// `content-end` with no matching `content-start`. Mirrors the Gemini writer's
    /// no-frame-for-untracked-index behavior.
    open_text_indices: std::collections::BTreeSet<usize>,
    /// THIS STREAM'S `id`, minted ONCE and replayed on every later `message-start`.
    ///
    /// A stream is not guaranteed to carry exactly one `MessageStart`: the Anthropic reader emits
    /// it 1:1 with the upstream frame rather than gating it, so a cohere-egress stream fed from an
    /// Anthropic ingress can see two. Without this cell the second synthesized a FRESH id, so one
    /// message announced itself twice under two different identities. A native Cohere stream's id
    /// is fixed for the message, so the first one wins.
    stream_id: Option<String>,
    /// Bounded answer anchors for streamed block-local citation offsets.
    citation_blocks: std::collections::BTreeMap<usize, CohereCitationBlock>,
    next_citation_index: usize,
    answer_chars: usize,
    citation_bytes: usize,
    citation_truncated: bool,
    translate_cap: usize,
}

#[derive(Clone)]
struct CohereCitationBlock {
    base: usize,
    end: usize,
    text: String,
    contiguous: bool,
}

/// Value-namespace constructor for [`CohereWriter`]. A `const` and a struct may share a name (they
/// live in the value and type namespaces respectively), so `Protocol::cohere()` can keep writing
/// the bare `CohereWriter` literal while the type now carries per-stream state.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const CohereWriter: CohereWriter = CohereWriter {
    stream: StreamCell::new(CohereStream {
        block_indices: crate::codec::proto_codec::EmittedBlockIndices::new(),
        open_tool_indices: std::collections::BTreeSet::new(),
        open_text_indices: std::collections::BTreeSet::new(),
        stream_id: None,
        citation_blocks: std::collections::BTreeMap::new(),
        next_citation_index: 0,
        answer_chars: 0,
        citation_bytes: 0,
        citation_truncated: false,
        translate_cap: crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }),
};

impl CohereWriter {
    fn begin_stream(&self) {
        let mut stream = self.stream.lock();
        if stream.block_indices.begin() {
            stream.open_tool_indices.clear();
            stream.open_text_indices.clear();
            stream.stream_id = None;
            stream.citation_blocks.clear();
            stream.next_citation_index = 0;
            stream.answer_chars = 0;
            stream.citation_bytes = 0;
            stream.citation_truncated = false;
        }
    }

    fn finish_stream(&self) {
        self.stream.lock().block_indices.finish();
    }

    fn next_citation_index(&self) -> Option<usize> {
        let mut stream = self.stream.lock();
        let index = stream.next_citation_index;
        stream.next_citation_index = index.checked_add(1)?;
        Some(index)
    }

    fn wire_index(&self, index: usize) -> usize {
        self.stream.lock().block_indices.wire(index)
    }

    /// THE STREAM'S `id`: the first one wins. Returns the id already captured for this stream if
    /// there is one, otherwise captures a successful `mint()`.
    fn carried_stream_id(
        &self,
        mint: impl FnOnce() -> Result<String, IrError>,
    ) -> Result<String, IrError> {
        if let Some(id) = self.stream.lock().stream_id.clone() {
            return Ok(id);
        }
        let minted = mint();
        let mut stream = self.stream.lock();
        if let Some(id) = &stream.stream_id {
            return Ok(id.clone());
        }
        let id = minted?;
        stream.stream_id = Some(id.clone());
        Ok(id)
    }

    /// Record that a `tool-call-start` frame was emitted at IR block `index`, so the matching
    /// `BlockStop` closes it with `tool-call-end` rather than `content-end`.
    fn mark_tool_open(&self, index: usize) -> usize {
        let mut stream = self.stream.lock();
        stream.open_tool_indices.insert(index);
        stream.block_indices.open(index)
    }

    /// Return true and forget `index` if it was a previously-opened tool-call block; false if no
    /// tool-call block was opened at `index` (e.g. a text block, whose `BlockStop` must emit
    /// `content-end`).
    fn take_tool_open(&self, index: usize) -> bool {
        self.stream.lock().open_tool_indices.remove(&index)
    }

    /// Record that a `content-start` frame was emitted for text block `index`, so the matching
    /// `BlockStop` emits `content-end` for it.
    fn mark_text_open(&self, index: usize) -> usize {
        let mut stream = self.stream.lock();
        stream.open_text_indices.insert(index);
        stream.block_indices.open(index)
    }

    fn record_citation_text(&self, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        let chars = text.chars().count();
        let mut stream = self.stream.lock();
        let base = stream.answer_chars;
        let end = base.saturating_add(chars);
        stream.answer_chars = end;
        if let Some(block) = stream.citation_blocks.get_mut(&index) {
            block.contiguous &= block.end == base;
            block.end = end;
        }
        if stream.citation_truncated
            || stream.citation_bytes.saturating_add(text.len()) > stream.translate_cap
        {
            stream.citation_truncated = true;
            return;
        }
        stream.citation_bytes += text.len();
        if let Some(block) = stream.citation_blocks.get_mut(&index) {
            block.text.push_str(text);
        } else {
            stream.citation_blocks.insert(
                index,
                CohereCitationBlock {
                    base,
                    end,
                    text: text.to_string(),
                    contiguous: true,
                },
            );
        }
    }

    fn citation_span(
        &self,
        index: usize,
        citation: &crate::codec::ir::IrCitation,
    ) -> Option<(i64, i64)> {
        let stream = self.stream.lock();
        let block = stream.citation_blocks.get(&index)?;
        if !block.contiguous {
            return None;
        }
        let text = if stream.citation_truncated {
            ""
        } else {
            block.text.as_str()
        };
        crate::codec::url_citation_wire::citation_span(text, block.base, citation)
    }

    fn stream_citation_representable(
        &self,
        index: usize,
        citation: &crate::codec::ir::IrCitation,
    ) -> bool {
        citation_representable(citation)
            && (citation
                .raw
                .as_ref()
                .is_some_and(|raw| raw.get(keys::START).is_some() || raw.get(SOURCES).is_some())
                || [&citation.cited_text, &citation.url, &citation.title]
                    .iter()
                    .any(|value| value.as_ref().is_some_and(|value| !value.is_empty()))
                || self.citation_span(index, citation).is_some())
    }

    /// Return true and forget `index` if a `content-start` was emitted for that text block; false if
    /// no text block opened at `index` (e.g. a Thinking block that carried no opening frame, whose
    /// `BlockStop` must emit nothing).
    fn take_text_open(&self, index: usize) -> bool {
        self.stream.lock().open_text_indices.remove(&index)
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/input_hardening_tests.rs"]
mod input_hardening_tests;

#[cfg(test)]
#[path = "tests/egress_media_regression_tests.rs"]
mod egress_media_regression_tests;

#[cfg(test)]
#[path = "tests/field_carry_tests.rs"]
mod field_carry_tests;

#[cfg(test)]
#[path = "tests/ir_mapping_tests.rs"]
mod ir_mapping_tests;

#[cfg(test)]
#[path = "tests/ir_round3_tests.rs"]
mod ir_round3_tests;

#[cfg(test)]
#[path = "tests/usage_census_tests.rs"]
mod usage_census_tests;

/// The answer slots on the Cohere wire, and the cache-read count row.
#[cfg(test)]
#[path = "tests/df_map_audit_tests.rs"]
mod df_map_audit_tests;

#[cfg(test)]
#[path = "tests/closure_regression_tests.rs"]
mod closure_regression_tests;

#[cfg(test)]
#[path = "tests/continuation_regression_tests.rs"]
mod continuation_regression_tests;

#[cfg(test)]
#[path = "tests/citation_index_regression_tests.rs"]
mod citation_index_regression_tests;

#[cfg(test)]
#[path = "tests/new_findings_tests.rs"]
mod new_findings_tests;

#[cfg(test)]
#[path = "tests/citation_scaling_tests.rs"]
mod citation_scaling_tests;

#[cfg(test)]
#[path = "tests/wire_index_tests.rs"]
mod wire_index_tests;
