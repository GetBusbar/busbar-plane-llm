// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Cohere v2 protocol reader/writer implementation.

use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use busbar_contract::http::StatusCode;
use busbar_contract::protocol::*;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
#[cfg(test)]
use busbar_contract::upstream::StatusClass;
// Relative path so it resolves both standalone and netted into core.
#[allow(unused_imports)]
// used standalone; redundant with the `busbar_contract::protocol::*` glob when netted into core
use super::proto_codec::*;
// Explicit import so it binds to this crate's own `proto_codec`, not the `busbar_contract::protocol::*` glob.
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
/// paths (`/v2/embed`, `/v2/rerank`, rung 9). Lower strength binds tighter.
fn claims(
    _h: busbar_contract::protocol::HeadFields<'_>,
    path: &str,
) -> Option<busbar_contract::protocol::ClaimStrength> {
    use busbar_contract::protocol::ClaimStrength;
    if path.ends_with("/v2/chat") || path.ends_with("/v1/chat") {
        return Some(ClaimStrength(8));
    }
    if path.ends_with("/v2/embed") || path.ends_with("/v2/rerank") {
        return Some(ClaimStrength(9));
    }
    None
}

/// Headerless residual detection: an exact `/v2/chat` names Cohere (rung 50).
fn residual_claims(path: &str) -> Option<busbar_contract::protocol::ClaimStrength> {
    if path == "/v2/chat" {
        return Some(busbar_contract::protocol::ClaimStrength(50));
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
    streaming_content_type: Some(busbar_contract::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: None,
    // Cohere tool ids are free-form with no canonical prefix. An empty prefix would make the
    // reversibility marker the only signal, colliding with client-authored ids and corrupting
    // tool_use/tool_result correlation cross-protocol, so ids pass through verbatim.
    native_tool_id_prefix: None,
    ingress_auth: IngressAuth::Bearer,
    // Plain `Authorization: Bearer <key>`, declared as data: the kernel's egress-auth unit presents
    // the lane credential and the key never passes through this plane.
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
    // Cohere v2's `citation-start` carries a single Citation at `delta.message.citations`
    // (docs.cohere.com/v2/docs/streaming), so a multi-citation IR delta must be fanned out to one
    // event per citation at the framing seam (`StreamTranslate`), as for Anthropic/Bedrock.
    max_citations_per_delta: Some(1),
    // Cohere Python SDK UA. Re-verify per release; `test_egress_ua_versions_are_pinned_and_present`
    // guards drift. Written on a translated route, on a same-dialect route whose caller sent no
    // user-agent, and on the health probe.
    egress_user_agent: "cohere-python/5.11.0",
    has_model_in_url: false,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::UNAUTHORIZED.as_u16(),
        busbar_contract::protocol::ERR_TYPE_AUTHENTICATION,
    ),
    ingress_relays_amzn_headers: false,
    ingress_relayed_response_header_names: &[],
    auth_failure_message: "invalid api token",
    uses_array_stream_shim: false,
    has_native_path_not_found: false,
    egress_stream_accept: busbar_contract::protocol::TEXT_EVENT_STREAM,
    // No model-discovery surface: `/v1/models` resolves to the OpenAI envelope (documented).
    models_list_envelope: None,
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: None,
    // No fingerprint header for the shared list-models surface.
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
/// "document"}]}`. The offsets are character offsets into the assembled content, the IR contract
/// ([`crate::codec::ir::IrCitation::start_index`]), so they carry with no conversion (unlike the
/// OpenAI-family `annotations`, whose unit is undocumented and not asserted).
///
/// This reader is why a Cohere backend's RAG grounding survives a hop: the writer emitted citations
/// while every reader site hardcoded `citations: Vec::new()`, so a citation into Cohere worked and
/// out of Cohere vanished (a missing reader, not a translation limit).
///
/// The source object is kept verbatim in `raw` so a same-protocol path re-emits it unchanged, as
/// the Anthropic reader does.
pub fn read_cohere_citations(citations: &serde_json::Value) -> Vec<crate::codec::ir::IrCitation> {
    let mut out = Vec::new();
    let Some(arr) = citations.as_array() else {
        return out;
    };
    for entry in arr {
        // A citation with neither a quoted span nor offsets carries no locatable claim: skipped, not synthesized.
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
        // The first source supplies the neutral title/url/document-index; the whole entry (all
        // sources) is preserved in `raw`.
        let first_source = entry
            .get(SOURCES)
            .and_then(|s| s.as_array())
            .and_then(|a| a.first());
        let doc = first_source.and_then(|s| s.get(keys::DOCUMENT));
        out.push(crate::codec::ir::IrCitation {
            domain: None,
            // Character spans into the answer text: Anthropic's `char_location`, the neutral `kind` vocabulary.
            kind: Some("char_location".to_string()),
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

/// Project a neutral [`crate::codec::ir::IrCitation`] into Cohere v2's citation object, the inverse
/// of [`read_cohere_citations`]. A citation this protocol read carries the source verbatim in
/// `raw`; re-emitting it is byte-exact and beats a field-by-field rebuild that would lose
/// Cohere-specific members. Only a citation with no `raw` (synthesized cross-protocol) is built from
/// the neutral fields.
fn write_cohere_citation(c: &crate::codec::ir::IrCitation) -> serde_json::Value {
    if let Some(raw) = &c.raw {
        // Re-emit `raw` only when it is a Cohere citation: a foreign raw (an Anthropic
        // `web_search_result_location`) would put a foreign shape on the Cohere wire.
        if raw.get(keys::START).is_some() || raw.get(SOURCES).is_some() {
            return raw.clone();
        }
    }
    let mut obj = serde_json::Map::new();
    if let Some(s) = c.start_index {
        obj.insert(keys::START.to_string(), serde_json::json!(s));
    }
    if let Some(e) = c.end_index {
        obj.insert(keys::END.to_string(), serde_json::json!(e));
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
    serde_json::Value::Object(obj)
}

/// The `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`] this protocol produces: a Cohere
/// v2 tool-result `document`, whose `data` is an arbitrary string map, not bytes with a mime type,
/// so it has no neutral base64/url form. Only this writer re-emits it; a foreign writer drops it with a warn.
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
// The prompt-cache hit count is a slice of the input count (never dropped); the model is read beside the id.
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
/// streaming unbounded distinct `tool-call-start` indices would grow it without bound. Past the cap
/// new frames are not recorded; no real Cohere v2 stream approaches it.
const MAX_TRACKED_TOOL_FRAMES: usize = 4096;

/// Sentinel recorded in `state.open_tools` the first time a text block opens, encoding that "a text
/// block has occupied IR index 0 this stream" so tool blocks stay off index 0 even after the text
/// block closes (`text_block_open` reverts on `content-end`).
///
/// `usize::MAX` cannot collide: every real `open_tools` entry is the clamped wire `frame_idx`,
/// bounded far below it by `MAX_TOOL_FRAME_INDEX`, so the sentinel is excluded from every scan.
/// Keeping it in the existing set keeps the fix inside this module (the shared `StreamDecodeState`
/// has no text-high-water field). The wire `index` is upstream-controlled, so every read site clamps
/// it (`clamp_frame_index`).
const TEXT_BLOCK_SEEN_SENTINEL: usize = usize::MAX;

/// Bound on the upstream-controlled stream-frame `index` at every tool-call read site, matching
/// `MAX_TRACKED_TOOL_FRAMES`: it keeps every recorded `frame_idx` far below
/// `TEXT_BLOCK_SEEN_SENTINEL` and leaves realistic streams (small sequential indices) alone.
/// Mirrors the OpenAI reader's `MAX_TOOL_INDEX`.
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
/// `citation-start` (docs.cohere.com/v2/docs/streaming). The writer's `write_response_events`
/// emits it so a streamed citation is bracketed natively, not left as a lone `citation-start`.
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

/// Read the upstream-controlled stream-frame `index`, defaulting to 0 when absent/non-numeric, and
/// clamp it to `MAX_TOOL_FRAME_INDEX` so the packed entry can never collide with the sentinel.
fn clamp_frame_index(data: &serde_json::Value) -> usize {
    data.get(keys::INDEX)
        .and_then(|i| i.as_u64())
        .unwrap_or(0)
        .min(MAX_TOOL_FRAME_INDEX) as usize
}

/// Normalize Cohere v2's `tool_choice` (a top-level enum string) into the IR union so a forced
/// directive survives the seam instead of degrading to `auto`. Cohere models only `REQUIRED` (must
/// call some tool) and `NONE`, with no `auto` literal (the default when omitted) and no way to pin
/// one tool, so an unrecognized or absent value is `None` and a targeted tool degrades to `REQUIRED`
/// on write. The reader only ever sees `REQUIRED`/`NONE`.
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
        // `json_object` may carry the schema directly under `json_schema`. An unrecognized `type` is free-form JSON.
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
/// `{type:"enabled", token_budget:N}` is `Budget(N)`; `{type:"enabled"}` with no or `null` budget is
/// `Dynamic`; `{type:"disabled"}` is `Off` (IR-09). Anything else (unknown type, non-`u32` budget) is `None`.
fn read_cohere_reasoning(
    v: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrReasoningAsk> {
    let t = v?.as_object()?;
    match t.get(keys::TYPE).and_then(|ty| ty.as_str()) {
        Some(keys::ENABLED) => {}
        // IR-09: reasoning off, which a reasoning-by-default model honors. Not the same as saying nothing.
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
        // IR-09: matched first; `Off` is a disable ask, never an enable with a budget.
        crate::codec::ir::IrReasoningAsk::Off => {
            serde_json::json!({ (keys::TYPE): keys::DISABLED })
        }
        // `Dynamic` has no table entry: it is Cohere's own budget-less enable.
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
/// IR-13). Every readable document becomes a base64 `text/plain` document, the one form every
/// dialect with a document slot carries (Anthropic `document`, Gemini `inlineData`, Bedrock
/// `document`, OpenAI `file`):
/// - a plain text document (`data` with a string `text` and at most a string `title`) carries its `text`;
/// - a bare string (a `documents` entry may be one) or string `data` carries that string;
/// - any other `data` is a JSON map of fields (`title`/`snippet`/`url`/..., which Cohere renders to
///   the model as text) and carries the map's JSON text. It once rode the opaque cohere `Vendor`
///   escape that every foreign writer drops, so a grounding document never reached a foreign model.
///
/// The name is the string `title`, else the `id`. Only a document with no readable content (no
/// `data`, or a non-string non-object `data`) keeps the `Vendor` escape: this writer re-emits it and
/// a foreign writer drops it with a warn.
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
/// image fidelity (IR-08). An unknown word is `None` with a warn, not coerced onto another fidelity.
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
/// follow-up). It is the tool's output, so it takes the tool-output carriers, not an attachment slot:
/// JSON `data` (object/array/number/bool) is an [`crate::codec::ir::IrBlock::Json`], string `data`
/// a `Text`, the forms every writer projects inside a tool result. The `id` is a Cohere citation
/// handle with no foreign analog. A document with no `data` keeps the opaque cohere `Vendor` escape.
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
/// this dialect's own `Vendor` document verbatim, or a base64 `text/*` document as
/// `{"data": {"text": <decoded>, "title"?: <name>}}` (inverse of [`read_cohere_document`]). `None`
/// for anything with no Cohere form: binary bytes (a PDF), a URL, a foreign vendor handle, bad base64.
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
/// [`crate::codec::ir::IrTokenLogprob`] (COH-13 / COH-14). Cohere reports per decoded text chunk; the
/// IR unit is a span with its log probability, so a chunk maps to its `text` with the sum of its
/// token logprobs (the joint probability of that text; for a one-token chunk, the token's own). A
/// chunk with no text, no logprobs or an unreadable one is skipped, never invented. Cohere reports
/// no alternatives, so `top` is empty.
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
        top: Vec::new(),
    })
}

/// Cohere v2 `finish_reason` to canonical [`crate::codec::ir::IrStopReason`]; the only read-side
/// place that knows the vocabulary. An unmodeled token is `Other`.
fn read_cohere_stop_reason(token: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match token {
        COHERE_FINISH_COMPLETE => S::EndTurn,
        COHERE_FINISH_MAX_TOKENS => S::MaxTokens,
        COHERE_FINISH_TOOL_CALL => S::ToolUse,
        COHERE_FINISH_STOP_SEQUENCE => S::StopSequence,
        // `ERROR_TOXIC` is the content-moderation stop; generic `ERROR` is an infra failure.
        COHERE_FINISH_ERROR_TOXIC => S::Safety,
        COHERE_FINISH_ERROR => S::Error,
        // `TIMEOUT` reads as `Other`: the returned answer is served and billed, whereas `Error`
        // would turn a buffered answer into a billed 502 and a lane fault (only stream `ERROR` is an error).
        _ => S::Other,
    }
}

/// Map a canonical IR stop reason to a valid Cohere v2 `finish_reason`. `ERROR` is first-class
/// (reader `ERROR` to `S::Error`, writer back). The reader lowercases only tokens it does not model
/// (`ERROR_LIMIT`, `USER_CANCEL`), which arrive as `S::Other` and degrade to `COMPLETE`, as does a
/// foreign token such as `refusal` (it would upper-case to `REFUSAL`, not a member of Cohere's enum,
/// which a strict client rejects). Exhaustive: `refusal`, `pause_turn` and `other` fall back to
/// `COMPLETE`.
///
/// `S::Safety` maps to `COMPLETE` (COH-15). A safety stop is a served response cut short or
/// withheld, not an upstream failure, and v2 `/v2/chat`'s enum
/// (`COMPLETE|STOP_SEQUENCE|MAX_TOKENS|TOOL_CALL|ERROR|TIMEOUT`) has no token for it (`ERROR_TOXIC`
/// is a v1 value). Mapping it to `ERROR` told a client its request had failed (retry, alert) when
/// the upstream had answered. `COMPLETE` is the projection `Refusal` already takes. The reader's
/// `ERROR_TOXIC` to `S::Safety` stays for a v1-dialect upstream.
fn write_cohere_stop_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::EndTurn => COHERE_FINISH_COMPLETE,
        S::StopSequence => COHERE_FINISH_STOP_SEQUENCE,
        S::MaxTokens => COHERE_FINISH_MAX_TOKENS,
        S::ToolUse => COHERE_FINISH_TOOL_CALL,
        S::Error => COHERE_FINISH_ERROR,
        S::Safety | S::Refusal | S::PauseTurn | S::Other => COHERE_FINISH_COMPLETE,
    }
}

/// Cohere's `usage.tokens.input_tokens` for an IR usage: the whole prompt. The IR's `input_tokens`
/// is the uncached share only, with cache read and write additive beside it (`IrUsage`'s
/// convention), whereas Cohere's is the whole prompt including the cached share, with the hit
/// reported separately as `usage.cached_tokens` (a slice this reader leaves inside the input count).
///
/// Copying the uncached count straight across told a Cohere client of a foreign backend a smaller
/// prompt than it sent: an OpenAI turn of 10 prompt tokens, 4 cached, reached it as
/// `input_tokens: 6` with no `cached_tokens`, and an Anthropic turn of 10 uncached + 4 cache-read +
/// 3 cache-written as `input_tokens: 10` (COH-10). This restores what the backend reported:
/// uncached, cache-read and cache-written summed. Cohere has no cache-write tier, so a cache write
/// is ordinary prompt input here.
fn cohere_prompt_tokens(usage: &crate::codec::ir::IrUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
}

/// Format 16 bytes as a UUID-shaped (8-4-4-4-12 lowercase hex) token. Native Cohere v2 chat ids are
/// bare RFC-4122 UUIDv4s with no prefix (version nibble `4` opening group 3, variant `10xx` opening
/// group 4), so a synthesized id must match to stay indistinguishable. The caller has already
/// stamped the version/variant bits.
fn format_uuid_layout(bytes: &[u8; 16]) -> String {
    // One allocation for the 32-char lowercase hex string.
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
/// earlier `secs << 32` layout leaked the server clock). There is no counter overlay: it would leave
/// predictable bytes a random v4 never has, and 122 random bits are collision-free in practice.
/// Never panics: on entropy failure the buffer stays zeroed and the stamping still yields a
/// well-formed (if non-random) v4.
fn synthesize_cohere_id(host: &dyn busbar_contract::codec::CodecHost) -> String {
    let mut bytes = [0u8; 16];
    // Host CSPRNG; failure is ignored (no panic on the request path), as stamping below still gives a valid v4.
    let _ = host.fill_entropy(&mut bytes);

    // RFC-4122 v4: byte 6 high nibble = 4 (group 3); byte 8 top two bits = 10 (group 4).
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format_uuid_layout(&bytes)
}

/// Whether a mid-stream `IrError`'s `provider_signal` names a content-moderation stop (so the
/// Cohere-ingress writer ends with `ERROR_TOXIC`, not generic `ERROR`). The IR `Error` arrives from
/// any upstream, so the text is not Cohere-specific: we recognise the canonical moderation tokens
/// busbar's readers normalise to (`safety`, the IR stop reason), the native Cohere `ERROR_TOXIC` and
/// the common provider words for a moderation or content-policy stop. Anything else is an
/// infrastructure-class error and maps to `ERROR`. An exhaustive boolean classifier with no
/// catch-all hiding an unhandled case.
fn cohere_error_is_content_moderation(signal: &str) -> bool {
    let s = signal.to_ascii_lowercase();
    s.contains("toxic")
        || s.contains("safety")
        || s.contains("moderation")
        || s.contains("content_policy")
        || s.contains("content-policy")
        || s.contains("content_filter")
}

/// Number of genuine tool frames currently recorded in `state.open_tools` (excludes the
/// `TEXT_BLOCK_SEEN_SENTINEL`). `open_tools` may also carry the text sentinel, so `len()` alone is
/// NOT the tool count — but since it holds at most one sentinel plus one entry per tracked tool, a
/// single O(log n) `contains` check (rather than a full O(n) scan-and-filter) is enough to correct
/// for it.
fn cohere_tracked_tool_count(state: &crate::codec::ir::StreamDecodeState) -> usize {
    state.open_tools.len() - usize::from(state.open_tools.contains(&TEXT_BLOCK_SEEN_SENTINEL))
}

/// Record a `tool-call-start` for wire `frame_idx`, ASSIGNING it a stable IR block index, and return
/// that index. Returns `None` (emit nothing) when the frame is a duplicate of one already open, or
/// when the per-stream cap is reached.
///
/// The assigned IR index is `base + tracked_tool_count`, where `base` is 1 if a text block has ever
/// occupied IR index 0 this stream (recorded via `TEXT_BLOCK_SEEN_SENTINEL`) else 0. Keying the base
/// on the persistent sentinel — not the live `text_block_open` flag, which `content-end` resets to
/// false before tools arrive — keeps tool blocks off the text block's index 0.
/// Keying the per-tool offset on INSERTION ORDER (the count of already-tracked tools) rather than the
/// wire-index rank makes the assignment independent of monotonic wire indices and immutable once
/// made: a later tool with a SMALLER wire `frame_idx` no longer retroactively shifts an earlier
/// tool's index. Neither `state.open_tools` nor `state.tool_ir_index` is ever shrunk for the
/// stream's lifetime, so a recorded frame — and the IR index assigned to it — survives until the
/// stream ends.
fn cohere_assign_tool_ir_index(
    state: &mut crate::codec::ir::StreamDecodeState,
    frame_idx: usize,
) -> Option<usize> {
    // Duplicate tool-call-start for a frame already open: no-op (do not re-assign or re-emit).
    if state.tool_ir_index.get(&frame_idx).copied().is_some() {
        return None;
    }
    let tracked = cohere_tracked_tool_count(state);
    // New frame past the cap: not tracked, emit nothing (bounds per-stream memory).
    if tracked >= MAX_TRACKED_TOOL_FRAMES {
        return None;
    }
    let base = usize::from(state.open_tools.contains(&TEXT_BLOCK_SEEN_SENTINEL));
    let ir_index = base + tracked;
    state.open_tools.insert(frame_idx);
    state.tool_ir_index.insert(frame_idx, ir_index);
    Some(ir_index)
}

/// Resolve the IR block index the text content block claims for THIS stream, assigning it on first
/// appearance and returning the same value verbatim thereafter. The index is claimed BY ORDER OF
/// FIRST APPEARANCE — the count of tool blocks already tracked this stream — NOT a hardcoded 0, so a
/// `tool-call-start` that arrives BEFORE the first `content-start`/`content-delta` (which claims IR
/// index 0 via `cohere_assign_tool_ir_index` when no text has been seen) does not collide with the
/// text block: the tool keeps 0 and the text block takes the next free slot. This mirrors the Gemini
/// reader's `state.text_index` index-by-first-appearance scheme (gemini.rs), reusing the same shared
/// `StreamDecodeState.text_index` field. Once `state.text_index` is `Some`, it is immutable for the
/// stream's lifetime (so the matching `BlockStop` on `content-end` closes the index that was
/// actually opened, even after later tools push the tracked count up). The persistent
/// `TEXT_BLOCK_SEEN_SENTINEL` still gates the TOOL base offset (so a tool opened after the text block
/// stays off the text index even after `content-end` clears the live flag); this
/// helper governs only the TEXT block's own index (the tool-before-text collision).
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
///
/// A Cohere reasoning model streams its reasoning as its OWN content block — `content-start`
/// `{type:"thinking"}`, `content-delta {thinking}`, `content-end` — BEFORE the answer's text content
/// block, which arrives as a second `content-start {type:"text"}`. The reader used to treat every
/// content block as THE text block: the thinking block claimed the one text slot, its `content-end`
/// latched `text_block_closed`, and every answer frame after it was dropped, so a reasoning model's
/// stream reached a foreign client with no answer text at all (COH-01). A thinking block therefore
/// takes an IR index of its own, allocated through the SAME insertion-order seam the tool calls use
/// (`cohere_assign_tool_ir_index`), so it can never collide with the text block or a tool block.
///
/// The keys sit far above `MAX_TOOL_FRAME_INDEX` (every genuine tool key is clamped to it) and
/// below `TEXT_BLOCK_SEEN_SENTINEL`, so they collide with neither. The n-th thinking block of the
/// stream is keyed `THINKING_FRAME_BASE + n`.
const THINKING_FRAME_BASE: usize = usize::MAX / 2;

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
) -> Option<usize> {
    let seen = state.tool_ir_index.range(THINKING_FRAME_BASE..).count();
    let index = cohere_assign_tool_ir_index(state, THINKING_FRAME_BASE.saturating_add(seen))?;
    state.thinking_block_open = true;
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
/// it) at the index it CLAIMED, and latch it closed so a later frame cannot reopen a stopped index.
fn cohere_close_text_slot(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if state.text_block_open {
        state.text_block_open = false;
        state.text_block_closed = true;
        // A tool plan open in the slot closes with it.
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
    /// exceeded") phrasing. Cohere has no structured context-length code/type, so this is a
    /// case-insensitive substring scan of the raw body. The phrases mirror the ones the
    /// `#[cfg(test)] classify()` helper recognizes ("too many tokens", "maximum"+"tokens") plus the
    /// broader provider wording ("input too long", "exceeds maximum context",
    /// "token limit" — matched via the "too long" / "exceeds"+"context" substrings), so production
    /// `extract_error` synthesizes the canonical
    /// `context_length_exceeded` code that the breaker maps to `StatusClass::ContextLength`.
    fn body_signals_context_length(body: &[u8]) -> bool {
        let lower = String::from_utf8_lossy(body).to_lowercase();
        lower.contains("too many tokens")
            // `too long` is co-constrained to a token/context/input qualifier so it only fires on a
            // genuine oversized-request error. A bare `contains("too long")` over-matched ANY
            // upstream message containing "too long" (e.g. "request URL too long", "value too long
            // for column"), mis-synthesizing the canonical `context_length_exceeded` code and
            // triggering a no-penalty ContextLength failover for an unrelated client error.
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
}

/// Value-namespace constructor for [`CohereWriter`]. A `const` and a struct may share a name (they
/// live in the value and type namespaces respectively), so `Protocol::cohere()` can keep writing
/// the bare `CohereWriter` literal while the type now carries per-stream state. Each USE of the
/// const inlines a fresh `CohereWriter` with an empty open-tool set, so every `Protocol::cohere()`
/// call mints independent per-stream state — exactly the per-stream scoping the open/close pairing
/// needs. `Mutex::new`/`BTreeSet::new` are const fns, so this is valid in const context.
///
/// `clippy::declare_interior_mutable_const` warns that a `const` with interior mutability is
/// inlined per use rather than shared. That per-use fresh instance is PRECISELY the semantics we
/// need: a `static` would share ONE open-tool set across every stream in the process, letting one
/// stream's tool index leak into another. So the lint's suggestion is wrong for this site and is
/// suppressed deliberately (mirrors the Responses writer's identically-shaped const).
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const CohereWriter: CohereWriter = CohereWriter {
    stream: StreamCell::new(CohereStream {
        open_tool_indices: std::collections::BTreeSet::new(),
        open_text_indices: std::collections::BTreeSet::new(),
        stream_id: None,
    }),
};

impl CohereWriter {
    /// THE STREAM'S `id`: the first one wins. Returns the id already captured for this stream if
    /// there is one, otherwise captures and returns `mint()`, so a duplicate `message-start`
    /// re-states the identity the client already has. Lock poisoning degrades to the freshly minted
    /// id rather than panicking on the request path.
    fn carried_stream_id(&self, mint: impl FnOnce() -> String) -> String {
        self.stream
            .lock()
            .stream_id
            .get_or_insert_with(mint)
            .clone()
    }

    /// Record that a `tool-call-start` frame was emitted at IR block `index`, so the matching
    /// `BlockStop` closes it with `tool-call-end` rather than `content-end`. Lock poisoning degrades
    /// to a no-op rather than panicking on the request path.
    fn mark_tool_open(&self, index: usize) {
        self.stream.lock().open_tool_indices.insert(index);
    }

    /// Return true and forget `index` if it was a previously-opened tool-call block; false if no
    /// tool-call block was opened at `index` (e.g. a text block, whose `BlockStop` must emit
    /// `content-end`). Lock poisoning degrades to `false` (treat as a text close) rather than
    /// panicking on the request path.
    fn take_tool_open(&self, index: usize) -> bool {
        self.stream.lock().open_tool_indices.remove(&index)
    }

    /// Record that a `content-start` frame was emitted for text block `index`, so the matching
    /// `BlockStop` emits `content-end` for it. Cross-protocol blocks that emit no opening frame
    /// (redacted thinking / Image) are never recorded, so their `BlockStop` stays silent. Lock poisoning
    /// degrades to a no-op rather than panicking on the request path.
    fn mark_text_open(&self, index: usize) {
        self.stream.lock().open_text_indices.insert(index);
    }

    /// Return true and forget `index` if a `content-start` was emitted for that text block; false if
    /// no text block opened at `index` (e.g. a Thinking block that carried no opening frame, whose
    /// `BlockStop` must emit nothing). Lock poisoning degrades to `false` (emit nothing) rather than
    /// panicking on the request path.
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
