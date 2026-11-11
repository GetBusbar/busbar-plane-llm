// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! OpenAI Responses API protocol reader/writer implementation.

use crate::codec::dialect::ir_parse_error;
use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use busbar_contract::http::StatusCode;

use crate::codec::dialect::{bearer_error_code, CODE_INVALID_API_KEY};
use crate::codec::framework::ERR_TYPE_OVERLOADED;
use crate::codec::framework::ERR_TYPE_SERVER_ERROR;
use crate::codec::protocol::*;
use crate::codec::protocol::{
    ERR_TYPE_AUTHENTICATION, ERR_TYPE_INSUFFICIENT_QUOTA, ERR_TYPE_INVALID_REQUEST,
    ERR_TYPE_NOT_FOUND, ERR_TYPE_PERMISSION, ERR_TYPE_RATE_LIMIT,
};
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;

#[allow(unused_imports)]
use super::proto_codec::*;
use crate::codec::usage_count::{CountRead, CountSlot, UsageCount};

#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
pub mod handler;
mod origin;
mod reader;
mod slots;
mod writer;

/// Build this dialect's wire codec. A fresh instance per resolution: `ResponsesWriter` carries
/// per-stream mutable state (`sequence`, `response_id`).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, ResponsesReader, ResponsesWriter)
}

/// Router detection for `/v1/responses` (rung 10).
fn claims(
    _h: crate::codec::protocol::HeadFields<'_>,
    path: &str,
) -> Option<crate::codec::protocol::ClaimStrength> {
    if path.ends_with("/v1/responses") {
        return Some(crate::codec::protocol::ClaimStrength(10));
    }
    None
}

/// Headerless residual detection: an exact `/v1/responses` (rung 60).
fn residual_claims(path: &str) -> Option<crate::codec::protocol::ClaimStrength> {
    if path == "/v1/responses" {
        return Some(crate::codec::protocol::ClaimStrength(60));
    }
    None
}

/// The `/v1/responses` declaration. Shares OpenAI's `call_…` tool-id shape but declares its own
/// name, since a metric label is a protocol's own.
pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::ResponsesRequestHandler),
    verbs: &[busbar_contract::operation::OpVerb::CHAT],
    #[cfg(test)]
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    streaming_content_type: Some(crate::codec::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: None,
    native_tool_id_prefix: Some("call_"),
    #[cfg(test)]
    ingress_auth: IngressAuth::Bearer,

    #[cfg(test)]
    egress_auth_headers: None,
    #[cfg(test)]
    egress_auth_lane_constant: false,
    #[cfg(test)]
    egress_scheme: Some(EgressScheme::bearer()),
    stream_usage_requires_opt_in: false,
    requires_max_tokens: false,
    #[cfg(test)]
    stop_sequence_cap: None,
    cache_markers_model_gated: false,
    fills_thought_signature: false,
    frame_after_message_start: None,
    max_cache_control_breakpoints: None,
    #[cfg(test)]
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: false,
    max_citations_per_delta: None,

    egress_user_agent: "OpenAI/Python 1.54.0",
    has_model_in_url: false,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::UNAUTHORIZED.as_u16(),
        crate::codec::protocol::ERR_TYPE_AUTHENTICATION,
    ),
    ingress_relayed_response_header_names: &[],
    auth_failure_message: AUTH_FAILURE_MSG,
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
        let w = ResponsesWriter;
        f(&w)
    },
    with_reader: |f| f(&ResponsesReader),
    leaf: &crate::codec::leaf_codec::LeafCodecs::NONE,
};

/// Largest wire item or content index accepted in a streaming event. Out-of-range
/// indices are rejected before casting, rather than aliased to a legitimate block.
const MAX_OUTPUT_INDEX: usize = 127;

/// Fallback `model` when the IR carries none (cross-protocol paths). Strict SDKs type
/// `Response.model` as a required string, and a real endpoint never omits it.
const DEFAULT_MODEL: &str = crate::codec::dialect::FALLBACK_MODEL;

/// Cap on distinct output indices tracked per stream (`open_tools` and the writer's open-item
/// sets), bounding memory against a backend that emits a unique `output_index` per event.
const MAX_OPEN_TOOLS: usize = crate::codec::dialect::MAX_OPEN_TOOL_CALLS;

/// Append `fragment` to the buffer at `index`, honouring both bounds: a new index is refused once
/// `MAX_OPEN_TOOLS` items accumulate, and a buffer stops growing at `cap`
/// ([`ResponsesWriter::accum_byte_cap`]).
fn append_capped(
    map: &mut std::collections::BTreeMap<usize, String>,
    index: usize,
    fragment: &str,
    cap: usize,
) -> bool {
    match map.get_mut(&index) {
        Some(buf) => {
            if buf.len().saturating_add(fragment.len()) <= cap {
                buf.push_str(fragment);
            } else {
                return false;
            }
        }
        None => {
            if map.len() >= MAX_OPEN_TOOLS || fragment.len() > cap {
                return false;
            }
            map.entry(index).or_default().push_str(fragment);
        }
    }
    true
}

/// Carried weight of one buffered token logprob: token string, bytes and every top alternative's.
fn logprob_bytes(lp: &crate::codec::ir::IrTokenLogprob) -> usize {
    let alt = |t: &crate::codec::ir::IrTopLogprob| {
        std::mem::size_of::<crate::codec::ir::IrTopLogprob>()
            .saturating_add(t.token.len())
            .saturating_add(t.bytes.as_ref().map_or(0, Vec::len))
    };
    std::mem::size_of::<crate::codec::ir::IrTokenLogprob>()
        .saturating_add(lp.token.len())
        .saturating_add(lp.bytes.as_ref().map_or(0, Vec::len))
        .saturating_add(lp.top.iter().map(alt).fold(0usize, usize::saturating_add))
}

/// Carried text weight of one buffered citation (the owned strings, which upstream controls). The
/// verbatim `raw` entry weighs its compact serialised length.
fn citation_bytes(c: &crate::codec::ir::IrCitation) -> usize {
    fn len(s: &Option<String>) -> usize {
        s.as_ref().map_or(0, String::len)
    }
    /// Counts bytes written to it.
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let raw = c.raw.as_ref().map_or(0, |v| {
        let mut n = Count(0);

        let _ = serde_json::to_writer(&mut n, v);
        n.0
    });
    std::mem::size_of::<crate::codec::ir::IrCitation>()
        .saturating_add(len(&c.kind))
        .saturating_add(len(&c.cited_text))
        .saturating_add(len(&c.title))
        .saturating_add(len(&c.url))
        .saturating_add(len(&c.encrypted_index))
        .saturating_add(raw)
}

/// One message item's buffered citations with their carried text weight, kept per append so
/// accumulation stays linear in what the stream delivers.
#[derive(Clone, Default)]
struct CitationAccum {
    /// In arrival order.
    items: Vec<crate::codec::ir::IrCitation>,
    /// The sum of [`citation_bytes`] over `items`.
    bytes: usize,
}

/// One message item's logprobs and their carried weight. Only the incoming batch is weighed.
#[derive(Clone, Default)]
struct LogprobAccum {
    items: Vec<crate::codec::ir::IrTokenLogprob>,
    bytes: usize,
}

/// Key offset for OPEN TEXT output indices inside the shared `StreamDecodeState::open_tools` set. A
/// stream can carry several message items, each at its own `output_index`, so a single
/// `text_block_open` bool cannot pair BlockStart/BlockStop per index.
const TEXT_INDEX_KEY_OFFSET: usize = 1_000;

/// Base62 alphabet for the opaque suffix of native ids ([`synthesize_item_id`], [`synthesize_response_id`]).
const BASE62: &[u8; 62] = crate::codec::dialect::BASE62_ALPHABET;

/// Width of the base62 suffix on a synthesized item id. Native ids carry a long opaque token, so 48
/// chars keeps a length- or regex-checking client from fingerprinting it as non-native.
const ITEM_ID_TOKEN_LEN: usize = 48;

/// Width of the base62 suffix on a synthesized `resp_` id (native ones are ~38+ chars).
const RESPONSE_ID_TOKEN_LEN: usize = 48;

/// SSE event type names on the `/v1/responses` wire.
const EVT_RESPONSE_CREATED: &str = "response.created";
const EVT_OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
const EVT_OUTPUT_ITEM_DONE: &str = "response.output_item.done";

const EVT_CONTENT_PART_ADDED: &str = "response.content_part.added";
const EVT_CONTENT_PART_DONE: &str = "response.content_part.done";
const EVT_OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";

const EVT_OUTPUT_TEXT_DONE: &str = "response.output_text.done";

const EVT_REFUSAL_DELTA: &str = "response.refusal.delta";
const EVT_REFUSAL_DONE: &str = "response.refusal.done";

const EVT_OUTPUT_TEXT_ANNOTATION_ADDED: &str = "response.output_text.annotation.added";
const EVT_FUNCTION_CALL_ARGS_DONE: &str = "response.function_call_arguments.done";
const EVT_FUNCTION_CALL_ARGS_DELTA: &str = "response.function_call_arguments.delta";
const EVT_REASONING_TEXT_DELTA: &str = "response.reasoning_text.delta";

const EVT_REASONING_TEXT_DONE: &str = "response.reasoning_text.done";

const EVT_REASONING_SUMMARY_PART_ADDED: &str = "response.reasoning_summary_part.added";
const EVT_REASONING_SUMMARY_TEXT_DELTA: &str = "response.reasoning_summary_text.delta";
const EVT_REASONING_SUMMARY_TEXT_DONE: &str = "response.reasoning_summary_text.done";
const EVT_REASONING_SUMMARY_PART_DONE: &str = "response.reasoning_summary_part.done";
const EVT_RESPONSE_COMPLETED: &str = "response.completed";
const EVT_RESPONSE_FAILED: &str = "response.failed";
const EVT_RESPONSE_INCOMPLETE: &str = "response.incomplete";
/// `include` entry asking a backend for per-token logprobs on every `output_text` part (the
/// Responses spelling of Chat `logprobs: true`).
const INCLUDE_OUTPUT_TEXT_LOGPROBS: &str = "message.output_text.logprobs";

const CALL_ID: &str = "call_id";
const CODE_INTERPRETER: &str = "code_interpreter";
const CONTENT_INDEX: &str = "content_index";
const ENCRYPTED_CONTENT: &str = "encrypted_content";
const FILE_URL: &str = "file_url";
const FILTERS: &str = "filters";
const FUNCTION_CALL_OUTPUT: &str = "function_call_output";
const INCLUDE: &str = "include";
const INCOMPLETE_DETAILS: &str = "incomplete_details";
const INPUT_FILE: &str = "input_file";
const INPUT_IMAGE: &str = "input_image";
const INPUT_TOKENS_DETAILS: &str = "input_tokens_details";
const ITEM: &str = "item";
const ITEM_ID: &str = "item_id";
const OUTPUT_INDEX: &str = "output_index";
const PART: &str = "part";
const PREVIOUS_RESPONSE_ID: &str = "previous_response_id";
const FIELD_STORE: &str = "store";
const SUMMARY: &str = "summary";
const SUMMARY_INDEX: &str = "summary_index";
const SUMMARY_TEXT: &str = "summary_text";
const TOP_P: &str = "top_p";

/// Internal `provider_signal` sentinel for a `response.failed` event with no recognizable
/// `error.code`/`error.type`.
const SIGNAL_RESPONSE_FAILED: &str = "response_failed";

/// Response status values on the `/v1/responses` wire.
const STATUS_IN_PROGRESS: &str = "in_progress";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";
const STATUS_INCOMPLETE: &str = "incomplete";

/// Output item `type` values on the `/v1/responses` wire.
pub const ITEM_TYPE_FUNCTION_CALL: &str = "function_call";
const ITEM_TYPE_MESSAGE: &str = keys::MESSAGE;
/// A provider-run web search's output item.
const ITEM_TYPE_WEB_SEARCH_CALL: &str = "web_search_call";
pub const ITEM_TYPE_REASONING: &str = keys::REASONING;

/// Content part `type` values on the `/v1/responses` wire.
pub const CONTENT_TYPE_OUTPUT_TEXT: &str = "output_text";
const CONTENT_TYPE_REASONING_TEXT: &str = "reasoning_text";
const CONTENT_TYPE_INPUT_TEXT: &str = "input_text";

/// `incomplete_details.reason` values on the `/v1/responses` wire.
const INCOMPLETE_REASON_MAX_OUTPUT: &str = "max_output_tokens";
const INCOMPLETE_REASON_CONTENT_FILTER: &str = "content_filter";
const INCOMPLETE_REASON_OTHER: &str = "other";

/// Top-level `object` field value and vendor tag for the Responses protocol.
const OBJ_RESPONSE: &str = keys::RESPONSE;
const VENDOR_NAME: &str = "responses";
/// Label this dialect's usage counts are logged under.
const COUNT_LABEL: &str = "openai_responses";

/// Synthesized id prefixes.
const RESPONSE_ID_PREFIX: &str = "resp_";
const ITEM_ID_PREFIX_MSG: &str = "msg";
const ITEM_ID_PREFIX_FC: &str = "fc";
const ITEM_ID_PREFIX_RS: &str = "rs";

/// OpenAI-family error `code` strings self-owned in this protocol module.
const ERR_CODE_RATE_LIMIT: &str = "rate_limit_exceeded";
const ERR_CODE_STRING_ABOVE_MAX: &str = "string_above_max_length";

/// Authentication failure message for `auth_failure_message`.
const AUTH_FAILURE_MSG: &str = "Incorrect API key provided.";
const ACCUMULATION_FAILURE_MSG: &str = "Responses stream accumulation limit exceeded";

/// Fill a fixed-width base62 token entirely from the OS CSPRNG, with no counter overlay (a counter
/// leaves constant high digits, a fingerprint a native random id never carries).
fn synth_token<const N: usize>(
    host: &dyn crate::codec::cells::CodecHost,
) -> Result<String, IrError> {
    struct MinWidth<const M: usize>;
    impl<const M: usize> MinWidth<M> {
        const OK: () = assert!(
            M >= 11 && M <= 64,
            "synth_token<N>: N must be 11..=64 base62 chars"
        );
    }
    let () = MinWidth::<N>::OK;
    let mut token = [b'0'; N];
    if !crate::codec::dialect::fill_base62(host, &mut token, BASE62) {
        return Err(entropy_error());
    }
    String::from_utf8(token.to_vec()).map_err(|_| entropy_error())
}

/// Mint a fresh per-output-item id (`msg_…`, `fc_…`) for the streaming writer, since IR block events
/// carry only the integer `output_index`.
fn synthesize_item_id(
    host: &dyn crate::codec::cells::CodecHost,
    prefix: &str,
) -> Result<String, IrError> {
    Ok(format!(
        "{prefix}_{}",
        synth_token::<ITEM_ID_TOKEN_LEN>(host)?
    ))
}

/// Build the Responses `usage` object from [`crate::codec::ir::IrUsage`] with every member the
/// official SDKs require: `total_tokens`, `input_tokens_details` (`cached_tokens`,
/// `cache_write_tokens`) and `output_tokens_details` (`reasoning_tokens`).
fn build_responses_usage(usage: &crate::codec::ir::IrUsage) -> serde_json::Value {
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
    let input_total = usage
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_write);
    let total = input_total.saturating_add(usage.output_tokens);
    let mut wire = serde_json::json!({
        (keys::INPUT_TOKENS): usage.wire_total(&["input", "cache_read", "cache_write"], input_total),
        (INPUT_TOKENS_DETAILS): {
            (keys::CACHED_TOKENS): usage.wire_count("cache_read", cache_read),
            (keys::CACHE_WRITE_TOKENS): usage.wire_count("cache_write", cache_write),
        },
        (keys::OUTPUT_TOKENS): usage.wire_count("output", usage.output_tokens),
        (keys::OUTPUT_TOKENS_DETAILS): {
            (keys::REASONING_TOKENS): usage.detail.reasoning_tokens.unwrap_or(0),
        },
        "total_tokens": usage.wire_total(&["input", "output", "cache_read", "cache_write"], total),
    });
    use crate::codec::ir::open_class::{CACHE_WRITE_12H_CLASS, CACHE_WRITE_30M_CLASS};
    if usage.detail.exact_counts.as_ref().is_some_and(|e| {
        e.contains_key(CACHE_WRITE_30M_CLASS) && e.contains_key(CACHE_WRITE_12H_CLASS)
    }) {
        wire[INPUT_TOKENS_DETAILS]["cache_write_tokens_details"] = serde_json::json!({
            "ttl_30m_tokens": usage.wire_count(CACHE_WRITE_30M_CLASS, 0),
            "ttl_12h_tokens": usage.wire_count(CACHE_WRITE_12H_CLASS, 0),
        });
    }
    wire
}

/// The `usage` object for a response that has counted nothing yet (the `response.created`
fn zero_responses_usage() -> serde_json::Value {
    build_responses_usage(&crate::codec::ir::IrUsage::default())
}

/// Fill the members the spec marks required that the writer cannot derive from the IR: the
/// request-echo fields (`instructions`, `tools`, `tool_choice`, `parallel_tool_calls`, `metadata`,
/// `temperature`, `top_p`) and nullable `incomplete_details`.
fn fill_required_response_members(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    echo: Option<&serde_json::Value>,
) {
    let echoed = |key: &str| {
        echo.and_then(|e| e.get(key))
            .filter(|v| !v.is_null())
            .cloned()
    };
    let defaults: [(&str, serde_json::Value); 8] = [
        (INCOMPLETE_DETAILS, serde_json::Value::Null),
        (
            keys::INSTRUCTIONS,
            echoed(keys::INSTRUCTIONS).unwrap_or(serde_json::Value::Null),
        ),
        (
            keys::TOOLS,
            echoed(keys::TOOLS).unwrap_or_else(|| serde_json::json!([])),
        ),
        (
            keys::TOOL_CHOICE,
            echoed(keys::TOOL_CHOICE).unwrap_or_else(|| serde_json::json!(keys::AUTO)),
        ),
        (
            keys::PARALLEL_TOOL_CALLS,
            echoed(keys::PARALLEL_TOOL_CALLS).unwrap_or(serde_json::Value::Bool(true)),
        ),
        (
            keys::METADATA,
            echoed(keys::METADATA).unwrap_or_else(|| serde_json::json!({})),
        ),
        (
            keys::TEMPERATURE,
            echoed(keys::TEMPERATURE).unwrap_or_else(|| serde_json::json!(1.0)),
        ),
        (
            TOP_P,
            echoed(TOP_P).unwrap_or_else(|| serde_json::json!(1.0)),
        ),
    ];
    for (key, value) in defaults {
        obj.entry(key.to_string()).or_insert(value);
    }
}

/// Neutral IR logprobs in the Responses `LogProb` shape on an `output_text` part, built by the
/// shared encoder (`crate::codec::logprob_wire`).
fn write_responses_part_logprobs(lps: &[crate::codec::ir::IrTokenLogprob]) -> serde_json::Value {
    if lps.is_empty() {
        return serde_json::json!([]);
    }
    crate::codec::logprob_wire::write_token_logprobs(lps)
        .get_mut(keys::CONTENT)
        .map(serde_json::Value::take)
        .unwrap_or_else(|| serde_json::json!([]))
}

/// Neutral IR logprobs in the `ResponseLogProb` shape on `output_text.delta` / `.done` events
/// (no `bytes`).
fn write_responses_event_logprobs(lps: &[crate::codec::ir::IrTokenLogprob]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = lps
        .iter()
        .map(|lp| {
            let top: Vec<serde_json::Value> = lp
                .top
                .iter()
                .map(|t| serde_json::json!({ (keys::TOKEN): t.token, (keys::LOGPROB): t.logprob }))
                .collect();
            serde_json::json!({
                (keys::TOKEN): lp.token,
                (keys::LOGPROB): lp.logprob,
                (keys::TOP_LOGPROBS): top,
            })
        })
        .collect();
    serde_json::Value::Array(entries)
}

/// A Responses `logprobs` array (`LogProb`, or `ResponseLogProb` without `bytes`) into IR entries.
fn read_responses_logprobs(v: Option<&serde_json::Value>) -> Vec<crate::codec::ir::IrTokenLogprob> {
    match v {
        Some(arr @ serde_json::Value::Array(entries)) if !entries.is_empty() => {
            crate::codec::logprob_wire::read_token_logprobs(Some(
                &serde_json::json!({ (keys::CONTENT): arr }),
            ))
        }
        _ => Vec::new(),
    }
}

/// Responses `reasoning.effort` word to the IR ask (RSP-14, IR-09). `none` is `Off` (not "said
/// nothing"); `xhigh` is `XHigh`; the others map 1:1.
fn read_responses_reasoning_effort(word: &str) -> Option<crate::codec::ir::IrReasoningAsk> {
    match word {
        keys::NONE_WORD => Some(crate::codec::ir::IrReasoningAsk::Off),
        other => crate::codec::ir::IrReasoningEffort::parse_extended(other)
            .map(crate::codec::ir::IrReasoningAsk::Effort),
    }
}

/// Synthesize a `resp_<opaque base62>` id for cross-protocol responses where the backend supplied
/// none.
fn synthesize_response_id(host: &dyn crate::codec::cells::CodecHost) -> Result<String, IrError> {
    Ok(format!(
        "{}{}",
        RESPONSE_ID_PREFIX,
        synth_token::<RESPONSE_ID_TOKEN_LEN>(host)?
    ))
}

/// Accumulate a `system`/`developer` input turn's content into `system_blocks` (feeding
/// `IrRequest.system`) so the prompt is not dropped cross-protocol.
fn push_system_content(
    system_blocks: &mut Vec<crate::codec::ir::IrBlock>,
    origins: &mut Vec<crate::codec::ir::IrBlockOrigin>,
    content: Option<&serde_json::Value>,
) {
    let mut push_text = |text: &str, source: &'static crate::codec::ir::IrBlockSource| {
        if !text.is_empty() {
            system_blocks.push(crate::codec::ir::IrBlock::text(text.to_string()));
            origins.push(crate::codec::ir::IrBlockOrigin::at(source));
        }
    };
    match content {
        Some(serde_json::Value::String(s)) => push_text(s, &origin::MESSAGE_STRING),
        Some(serde_json::Value::Array(arr)) => {
            for block in arr {
                if let Some(text) = block.get(keys::TEXT).and_then(|t| t.as_str()) {
                    push_text(text, &origin::SYSTEM_PART_TEXT);
                }
            }
        }
        _ => {}
    }
}

/// IR content blocks for a message item's `content`, which is either a typed-block array or a bare
/// string shorthand.
fn message_content_blocks(
    content: Option<&serde_json::Value>,
) -> Option<(
    Vec<crate::codec::ir::IrBlock>,
    Vec<crate::codec::ir::IrBlockOrigin>,
)> {
    match content {
        Some(serde_json::Value::String(s)) => Some((
            vec![crate::codec::ir::IrBlock::text(s.clone())],
            vec![crate::codec::ir::IrBlockOrigin::at(&origin::MESSAGE_STRING)],
        )),
        Some(serde_json::Value::Array(arr)) => {
            Some(responses_parts_with_origins(arr, origin::PartSite::Message))
        }
        _ => None,
    }
}

/// Normalize the Responses `tool_choice` into the IR union. String forms match Chat, but the
/// targeted object is flat (`{"type":"function","name":"X"}`); the nested Chat shape is accepted as
/// a fallback.
fn read_responses_tool_choice(
    val: Option<&serde_json::Value>,
) -> (Option<crate::codec::ir::IrToolChoice>, Option<Vec<String>>) {
    let Some(val) = val else {
        return (None, None);
    };
    if let Some((choice, names)) = val.as_object().and_then(slots::read_allowed_tools) {
        return (Some(choice), Some(names));
    }
    (read_responses_tool_choice_directive(val), None)
}

/// The single-directive forms of [`read_responses_tool_choice`].
fn read_responses_tool_choice_directive(
    val: &serde_json::Value,
) -> Option<crate::codec::ir::IrToolChoice> {
    match val {
        serde_json::Value::String(s) => match s.as_str() {
            keys::AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
            keys::NONE_WORD => Some(crate::codec::ir::IrToolChoice::None),
            keys::REQUIRED => Some(crate::codec::ir::IrToolChoice::Required),
            _ => None,
        },
        serde_json::Value::Object(o) => {
            if o.get(keys::TYPE).and_then(|t| t.as_str()) == Some(keys::FUNCTION) {
                o.get(keys::NAME)
                    .and_then(|n| n.as_str())
                    .or_else(|| {
                        o.get(keys::FUNCTION)
                            .and_then(|f| f.get(keys::NAME))
                            .and_then(|n| n.as_str())
                    })
                    .map(|name| crate::codec::ir::IrToolChoice::Tool {
                        name: name.to_string(),
                    })
            } else {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire(keys::TOOL_CHOICE),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [tool_choice_type = o.get(keys::TYPE).and_then(|t| t.as_str()).unwrap_or(""),],
                    "dropping Responses tool_choice on ir parse: this tool_choice form has no IR \
                     carrier; the backend's default tool choice applies"
                );
                None
            }
        }
        _ => None,
    }
}

/// Emit the IR tool-choice union natively: strings for auto/none/required, the flat
/// `{"type":"function","name":...}` for a targeted tool.
fn write_responses_tool_choice(tc: &crate::codec::ir::IrToolChoice) -> serde_json::Value {
    match tc {
        crate::codec::ir::IrToolChoice::Auto => serde_json::json!(keys::AUTO),
        crate::codec::ir::IrToolChoice::None => serde_json::json!(keys::NONE_WORD),
        crate::codec::ir::IrToolChoice::Required => serde_json::json!(keys::REQUIRED),
        crate::codec::ir::IrToolChoice::Tool { name } => {
            serde_json::json!({(keys::TYPE): keys::FUNCTION, (keys::NAME): name})
        }
    }
}

/// Map a terminal `response.failed` provider signal (`error.code`/`error.type`) to the breaker
/// `StatusClass`.
fn class_for_response_failed(signal: &str) -> StatusClass {
    match signal {
        CODE_INVALID_API_KEY | ERR_TYPE_AUTHENTICATION => StatusClass::Auth,
        ERR_CODE_RATE_LIMIT => StatusClass::RateLimit,
        ERR_TYPE_INSUFFICIENT_QUOTA => StatusClass::Billing,
        crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH | ERR_CODE_STRING_ABOVE_MAX => {
            StatusClass::ContextLength
        }
        "invalid_prompt" => StatusClass::ClientError,
        ERR_TYPE_SERVER_ERROR | ERR_TYPE_OVERLOADED => StatusClass::ServerError,
        other => {
            let _ = other;
            StatusClass::ServerError
        }
    }
}

/// The pinned ResponseError enum (`responses.wire.json`, response `error.code`). This differs
/// from the HTTP error envelope's open-ended code/type strings.
fn is_response_error_code(signal: &str) -> bool {
    matches!(
        signal,
        "bio_policy"
            | "data_residency_mismatch"
            | "empty_image_file"
            | "failed_to_download_image"
            | "image_content_policy_violation"
            | "image_file_not_found"
            | "image_file_too_large"
            | "image_parse_error"
            | "image_too_large"
            | "image_too_small"
            | "invalid_base64_image"
            | "invalid_image"
            | "invalid_image_format"
            | "invalid_image_mode"
            | "invalid_image_url"
            | "invalid_prompt"
            | "misalignment_policy_violation"
            | ERR_CODE_RATE_LIMIT
            | ERR_TYPE_SERVER_ERROR
            | "unsupported_image_media_type"
            | "vector_store_timeout"
    )
}

/// Keep a native ResponseError code; otherwise project the IR class into that closed enum.
fn responses_error_code(err: &crate::codec::protocol::IrError) -> String {
    if let Some(s) = err.provider_signal.as_deref() {
        if is_response_error_code(s) {
            return s.to_string();
        }
    }
    let code = match err.class {
        StatusClass::RateLimit => ERR_CODE_RATE_LIMIT,
        StatusClass::ContextLength | StatusClass::ClientError => "invalid_prompt",
        StatusClass::Auth
        | StatusClass::Billing
        | StatusClass::Overloaded
        | StatusClass::ServerError
        | StatusClass::Timeout
        | StatusClass::Network => ERR_TYPE_SERVER_ERROR,
    };
    if err.provider_signal.is_some()
        || matches!(
            err.class,
            StatusClass::Auth | StatusClass::Billing | StatusClass::ContextLength
        )
    {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::wire("error.code"),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [class = ?err.class, ],
            "Responses ResponseError cannot carry the original error code or subtype; \
             emitting a canonical code and retaining the provider signal in the message"
        );
    }
    code.to_string()
}

#[derive(Clone)]
pub struct ResponsesReader;

#[cfg(test)]
include!("tests/mod_responses_block_helper.rs");

fn responses_parts_with_origins(
    arr: &[serde_json::Value],
    site: origin::PartSite,
) -> (
    Vec<crate::codec::ir::IrBlock>,
    Vec<crate::codec::ir::IrBlockOrigin>,
) {
    let mut blocks = Vec::new();
    let mut origins = Vec::new();
    for value in arr {
        if let Ok((block, origin)) = responses_block_with_origin(value, site) {
            blocks.push(block);
            origins.push(origin);
        }
    }
    (blocks, origins)
}

fn responses_block_with_origin(
    block_val: &serde_json::Value,
    site: origin::PartSite,
) -> Result<(crate::codec::ir::IrBlock, crate::codec::ir::IrBlockOrigin), IrError> {
    let obj = block_val.as_object().ok_or_else(ir_parse_error)?;

    let block_type = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");

    let block = match block_type {
        CONTENT_TYPE_INPUT_TEXT | CONTENT_TYPE_OUTPUT_TEXT => {
            let text_val = obj.get(keys::TEXT);
            let text = text_val.and_then(|t| t.as_str()).unwrap_or("").to_string();

            let citations = obj
                .get(keys::ANNOTATIONS)
                .map(super::url_citation_wire::read_url_annotations)
                .unwrap_or_default();
            Ok(crate::codec::ir::IrBlock::Text {
                text,
                cache_control: None,
                citations,
                refusal: false,
            })
        }
        INPUT_IMAGE => responses_input_image_block(block_val).ok_or_else(ir_parse_error),

        INPUT_FILE => {
            let name = obj
                .get(keys::FILENAME)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let source = if let Some(data_uri) = obj
                .get(keys::FILE_DATA)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                super::ir_encode::parse_image_url(data_uri)
            } else if let Some(url) = obj
                .get(FILE_URL)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                crate::codec::ir::IrImageSource::Url(url.to_string())
            } else {
                crate::codec::ir::IrImageSource::Vendor {
                    vendor: VENDOR_NAME,
                    value: serde_json::json!({
                        (keys::FILE_ID): obj.get(keys::FILE_ID).and_then(|v| v.as_str()).unwrap_or("")
                    }),
                }
            };
            let kind = match &source {
                crate::codec::ir::IrImageSource::Base64 { media_type, .. } => {
                    crate::codec::ir::IrMediaKind::from_media_type(media_type)
                }
                _ => crate::codec::ir::IrMediaKind::Document,
            };
            Ok(crate::codec::ir::IrBlock::Media {
                kind,
                source,
                name,
                cache_control: None,
                citations: None,
                context: None,
            })
        }

        keys::REFUSAL => Ok(crate::codec::ir::IrBlock::Text {
            text: obj
                .get(keys::REFUSAL)
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            cache_control: None,
            citations: Vec::new(),
            refusal: true,
        }),

        keys::INPUT_AUDIO => {
            let audio_obj = obj.get(keys::INPUT_AUDIO).ok_or_else(ir_parse_error)?;
            let data = audio_obj
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let format = audio_obj
                .get(keys::FORMAT)
                .and_then(|v| v.as_str())
                .unwrap_or("wav");
            Ok(crate::codec::ir::IrBlock::Media {
                kind: crate::codec::ir::IrMediaKind::Audio,
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: format!("audio/{format}"),
                    data,
                },
                name: None,
                cache_control: None,
                citations: None,
                context: None,
            })
        }

        _ => Err(ir_parse_error()),
    }?;
    Ok((block, origin::part(site, block_type)))
}

/// The request content grammar (`codec::drops`): a message item's `content[]` and a tool output
/// item's `output[]`.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[
    crate::codec::drops::Blocks {
        at: &["input[]", "content[]"],
        tag: Some(keys::TYPE),
        modelled: INPUT_KINDS,
        companions: &[],
    },
    crate::codec::drops::Blocks {
        at: &["input[]", "output[]"],
        tag: Some(keys::TYPE),
        modelled: INPUT_KINDS,
        companions: &[],
    },
];

/// The content kinds `responses_block` reads.
const INPUT_KINDS: &[&str] = &[
    CONTENT_TYPE_INPUT_TEXT,
    CONTENT_TYPE_OUTPUT_TEXT,
    INPUT_IMAGE,
    INPUT_FILE,
    keys::REFUSAL,
    keys::INPUT_AUDIO,
];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, "type=input_text"),
    (crate::codec::drops::kind::IMAGE, "type=input_image"),
    (crate::codec::drops::kind::DOCUMENT, "type=input_file"),
    (crate::codec::drops::kind::THINKING, "type=reasoning"),
    (crate::codec::drops::kind::TOOL_USE, "type=function_call"),
    (
        crate::codec::drops::kind::TOOL_RESULT,
        "type=function_call_output",
    ),
];

/// IR request members the reader carries by code from a path no map-file row names.
const REQUEST_CODE_NAMES: &[(&str, &str)] = &[
    (crate::codec::drops::name::RESPONSE_FORMAT, "text.format"),
    (crate::codec::drops::name::TOP_LOGPROBS, keys::TOP_LOGPROBS),
];

const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["output[]", "content[]"],
    tag: Some(keys::TYPE),
    modelled: &[CONTENT_TYPE_OUTPUT_TEXT, keys::REFUSAL],
    companions: &[],
}];

/// What the reader parks in `extra` beside the members its map file does not model.
const PARKED: &[crate::codec::drops::Parked] = &[
    crate::codec::drops::Parked {
        key: keys::MODEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::TEXT,
        holds: crate::codec::drops::Holds::Members(&[]),
    },
    crate::codec::drops::Parked {
        key: keys::REASONING,
        holds: crate::codec::drops::Holds::Members(&[keys::EFFORT]),
    },
];

/// What this dialect's answers carry beyond its map file's rows (drop walk, design F3 "Drops").
const RESPONSE_CODE: &[&str] = &[];
const STREAM_CODE: &[&str] = &[
    "type=error",
    "type=response.queued",
    "type=response.function_call_arguments.done",
    "type=response.reasoning_text.done",
    "type=response.reasoning_summary_text.done",
    "type=response.reasoning_summary_part.added",
    "type=response.reasoning_summary_part.done",
    "type=response.output_text.annotation.added",
    "type=response.refusal.delta",
    "type=response.refusal.done",
];

/// Answer paths inside a carried subtree that its code does not carry (where a coarse map row
/// covers them).
pub(super) const ANSWER_DEFAULTS: &[(&str, &str)] = &[
    (keys::TOOLS, "[]"),
    (keys::TOOL_CHOICE, "\"auto\""),
    (keys::PARALLEL_TOOL_CALLS, "true"),
];

const RESPONSE_DROPS: &[&str] = &[
    "error.misalignment",
    "instructions[].type=additional_tools",
    "instructions[].type=apply_patch_call",
    "instructions[].type=apply_patch_call_output",
    "instructions[].type=code_interpreter_call",
    "instructions[].type=compaction",
    "instructions[].type=compaction_trigger",
    "instructions[].type=computer_call",
    "instructions[].type=computer_call_output",
    "instructions[].type=configuration_update",
    "instructions[].type=custom_tool_call",
    "instructions[].type=custom_tool_call_output",
    "instructions[].type=file_search_call",
    "instructions[].type=function_call.async",
    "instructions[].type=function_call.caller",
    "instructions[].type=function_call.namespace",
    "instructions[].type=function_call_output.caller",
    "instructions[].type=function_call_output.namespace",
    "instructions[].type=function_call_output.output[].type=input_file.prompt_cache_breakpoint",
    "instructions[].type=function_call_output.output[].type=input_image.prompt_cache_breakpoint",
    "instructions[].type=function_call_output.output[].type=input_text.prompt_cache_breakpoint",
    "instructions[].type=image_generation_call",
    "instructions[].type=local_shell_call",
    "instructions[].type=local_shell_call_output",
    "instructions[].type=mcp_approval_request",
    "instructions[].type=mcp_approval_response",
    "instructions[].type=mcp_call",
    "instructions[].type=mcp_list_tools",
    "instructions[].type=message.content[].type=input_file.prompt_cache_breakpoint",
    "instructions[].type=message.content[].type=input_image.prompt_cache_breakpoint",
    "instructions[].type=message.content[].type=input_text.prompt_cache_breakpoint",
    "instructions[].type=message.content[].type=output_text.annotations[].type=container_file_citation",
    "instructions[].type=message.content[].type=output_text.annotations[].type=file_citation",
    "instructions[].type=message.content[].type=output_text.annotations[].type=file_path",
    "instructions[].type=message.phase",
    "instructions[].type=program",
    "instructions[].type=program_output",
    "instructions[].type=shell_call",
    "instructions[].type=shell_call_output",
    "instructions[].type=tool_search_call",
    "instructions[].type=tool_search_output",
    "instructions[].type=web_search_call",
    "output[].type=additional_tools",
    "output[].type=apply_patch_call",
    "output[].type=apply_patch_call_output",
    "output[].type=code_interpreter_call",
    "output[].type=compaction",
    "output[].type=computer_call",
    "output[].type=computer_call_output",
    "output[].type=custom_tool_call",
    "output[].type=custom_tool_call_output",
    "output[].type=file_search_call",
    "output[].type=function_call.async",
    "output[].type=function_call.caller",
    "output[].type=function_call.namespace",
    "output[].type=function_call_output.caller",
    "output[].type=function_call_output.created_by",
    "output[].type=function_call_output.namespace",
    "output[].type=function_call_output.output[].type=input_file.prompt_cache_breakpoint",
    "output[].type=function_call_output.output[].type=input_image.prompt_cache_breakpoint",
    "output[].type=function_call_output.output[].type=input_text.prompt_cache_breakpoint",
    "output[].type=image_generation_call",
    "output[].type=local_shell_call",
    "output[].type=local_shell_call_output",
    "output[].type=mcp_approval_request",
    "output[].type=mcp_approval_response",
    "output[].type=mcp_call",
    "output[].type=mcp_list_tools",
    "output[].type=message.content[].type=output_text.annotations[].type=container_file_citation.container_id",
    "output[].type=message.phase",
    "output[].type=program",
    "output[].type=program_output",
    "output[].type=shell_call",
    "output[].type=shell_call_output",
    "output[].type=tool_search_call",
    "output[].type=tool_search_output",
    "output[].type=web_search_call.action.type=find_in_page",
    "output[].type=web_search_call.action.type=open_page",
    "output[].type=web_search_call.action.type=search.queries",
];

const STREAM_DROPS: &[&str] = &[
    "output[].type=message.content[].type=output_text.annotations[].type=container_file_citation",
    "output[].type=message.content[].type=output_text.annotations[].type=file_citation",
    "output[].type=message.content[].type=output_text.annotations[].type=file_path",
    "output[].type=web_search_call",
    "type=response.output_text.annotation.added.annotation_index",
];
const STREAM_REROOT: crate::codec::drops::Reroot = crate::codec::drops::Reroot {
    at: &[
        (
            "type=response.content_part.added.part",
            "output[].type=message.content[]",
        ),
        (
            "type=response.content_part.done.part",
            "output[].type=message.content[]",
        ),
        ("type=response.output_item.added.item", "output[]"),
        ("type=response.output_item.done.item", "output[]"),
    ],
    answer_drops: RESPONSE_DROPS,
};

/// Build an IR `Image` from a Responses `input_image` object. Prefers inline `image_url` (via
/// `parse_image_url`), else an uploaded-file reference as the typed `FileId` source so the writer
/// restores `file_id`.
fn responses_input_image_block(item: &serde_json::Value) -> Option<crate::codec::ir::IrBlock> {
    let detail = slots::read_image_detail(item);
    let image_url = item.get(keys::IMAGE_URL).and_then(|u| u.as_str());
    if let Some(url) = image_url.filter(|u| !u.is_empty()) {
        return Some(crate::codec::ir::IrBlock::Image {
            source: super::ir_encode::parse_image_url(url),
            cache_control: None,
            detail,
        });
    }
    if let Some(file_id) = item
        .get(keys::FILE_ID)
        .and_then(|f| f.as_str())
        .filter(|f| !f.is_empty())
    {
        return Some(crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Vendor {
                vendor: VENDOR_NAME,
                value: serde_json::json!({ (keys::FILE_ID): file_id }),
            },
            cache_control: None,
            detail,
        });
    }
    None
}

/// Chain-of-thought text from a Responses `reasoning` item: every `text` in `content[]`
pub fn read_reasoning_text(item: &serde_json::Value) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let mut acc: Option<Cow<'_, str>> = None;
    for (arr_key, type_key) in [
        (keys::CONTENT, CONTENT_TYPE_REASONING_TEXT),
        (SUMMARY, SUMMARY_TEXT),
    ] {
        if let Some(arr) = item.get(arr_key).and_then(|c| c.as_array()) {
            for part in arr {
                let type_ok = part
                    .get(keys::TYPE)
                    .and_then(|t| t.as_str())
                    .is_none_or(|t| t == type_key);
                if !type_ok {
                    continue;
                }
                let Some(t) = part.get(keys::TEXT).and_then(|t| t.as_str()) else {
                    continue;
                };
                acc = Some(match acc {
                    None => Cow::Borrowed(t),
                    Some(Cow::Borrowed(prev)) => {
                        let mut owned = String::with_capacity(prev.len() + t.len());
                        owned.push_str(prev);
                        owned.push_str(t);
                        Cow::Owned(owned)
                    }
                    Some(Cow::Owned(mut owned)) => {
                        owned.push_str(t);
                        Cow::Owned(owned)
                    }
                });
            }
        }
    }
    acc.unwrap_or(Cow::Borrowed(""))
}

/// The opaque `encrypted_content` blob of a `reasoning` item: a non-empty string, else `None`. The
/// reader drops an item with no text and no blob.
pub fn read_reasoning_encrypted_content(item: &serde_json::Value) -> Option<&str> {
    item.get(ENCRYPTED_CONTENT)
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
}

/// Responses `incomplete_details.reason` to [`crate::codec::ir::IrStopReason`]; unmodeled is `Other`.
fn read_responses_incomplete_reason(reason: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        INCOMPLETE_REASON_MAX_OUTPUT => S::MaxTokens,
        INCOMPLETE_REASON_CONTENT_FILTER => S::Safety,
        keys::REFUSAL => S::Refusal,
        _ => S::Other,
    }
}

/// [`crate::codec::ir::IrStopReason`] to terminal `status`. Only `max_tokens` and `safety` are
/// `incomplete`; Error is `failed`, and the rest (incl. tool_use, refusal) are `completed`.
fn write_responses_status(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::MaxTokens | S::Safety => STATUS_INCOMPLETE,
        S::Error => STATUS_FAILED,
        S::EndTurn | S::StopSequence | S::ToolUse | S::Refusal | S::PauseTurn | S::Other => {
            STATUS_COMPLETED
        }
    }
}

/// [`crate::codec::ir::IrStopReason`] to `incomplete_details.reason` (consulted only for
/// `MaxTokens`/`Safety`; others give `other`).
fn write_responses_incomplete_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::MaxTokens => INCOMPLETE_REASON_MAX_OUTPUT,
        S::Safety => INCOMPLETE_REASON_CONTENT_FILTER,
        S::EndTurn
        | S::StopSequence
        | S::ToolUse
        | S::Refusal
        | S::PauseTurn
        | S::Error
        | S::Other => INCOMPLETE_REASON_OTHER,
    }
}

/// Normalize a Responses `text.format` into the IR's canonical `response_format`. Responses uses a
/// flat json_schema shape, the IR (as the Chat reader stores it) nests it under `json_schema`.
fn read_text_format(
    text_val: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let format = text_val.and_then(|t| t.get(keys::FORMAT))?;
    let o = format.as_object()?;
    match o.get(keys::TYPE).and_then(|t| t.as_str()) {
        Some(keys::TEXT) => Some(crate::codec::ir::IrResponseFormat::bare(false, None)),

        Some(keys::JSON_SCHEMA) => {
            Some(crate::codec::ir::IrResponseFormat::from_json_schema_members(Some(format)))
        }

        Some(_) => Some(crate::codec::ir::IrResponseFormat::bare(true, None)),
        None => None,
    }
}

/// Project [`crate::codec::ir::IrResponseFormat`] into a `text.format` object (inverse of
/// [`read_text_format`]); the json_schema form is flat.
fn write_text_format(rf: &crate::codec::ir::IrResponseFormat) -> serde_json::Value {
    if !rf.json {
        return serde_json::json!({(keys::TYPE): keys::TEXT});
    }
    match &rf.schema {
        Some(schema) => {
            let mut f = serde_json::Map::new();
            f.insert(keys::TYPE.to_string(), serde_json::json!(keys::JSON_SCHEMA));
            rf.write_json_schema_members(schema, &mut f);
            serde_json::Value::Object(f)
        }
        None => serde_json::json!({(keys::TYPE): "json_object"}),
    }
}

mod cache_writes;
pub(crate) mod tool_usage;

/// The Responses usage counts, as data. `input_tokens` includes the cached prefix, so
/// `input_tokens_details.cached_tokens` is subtracted (saturating) to leave uncached input and
/// carried as the additive cache read (`None` when absent, never `Some(0)`).
const USAGE: &[UsageCount] = &[
    (
        CountSlot::CacheWrite30m,
        CountRead::Opt(&[
            INPUT_TOKENS_DETAILS,
            "cache_write_tokens_details",
            "ttl_30m_tokens",
        ]),
    ),
    (
        CountSlot::CacheWrite12h,
        CountRead::Opt(&[
            INPUT_TOKENS_DETAILS,
            "cache_write_tokens_details",
            "ttl_12h_tokens",
        ]),
    ),
    (CountSlot::Input, CountRead::Zero(&[keys::INPUT_TOKENS])),
    (
        CountSlot::Input,
        CountRead::Less(&[INPUT_TOKENS_DETAILS, keys::CACHED_TOKENS]),
    ),
    (
        CountSlot::Input,
        CountRead::Less(&[INPUT_TOKENS_DETAILS, keys::CACHE_WRITE_TOKENS]),
    ),
    (
        CountSlot::CacheWrite,
        CountRead::Opt(&[INPUT_TOKENS_DETAILS, keys::CACHE_WRITE_TOKENS]),
    ),
    (CountSlot::Output, CountRead::Zero(&[keys::OUTPUT_TOKENS])),
    (
        CountSlot::CacheRead,
        CountRead::Opt(&[INPUT_TOKENS_DETAILS, keys::CACHED_TOKENS]),
    ),
    (
        CountSlot::Reasoning,
        CountRead::Lenient(&[keys::OUTPUT_TOKENS_DETAILS, keys::REASONING_TOKENS]),
    ),
];

/// Identity [`read_responses_usage`] checks `usage.total_tokens` against
/// ([`crate::codec::ir::UsageIdentityNote::identity`]).
const RESPONSES_USAGE_IDENTITY: &str = "openai_responses.usage";

/// A Responses `usage` object (`None` when absent) to IR usage through [`USAGE`]; the served tier
/// (RSP-17) is a word on `response`, not a count.
fn read_responses_usage(
    usage: Option<&serde_json::Value>,
    response: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let mut ir = crate::codec::usage_count::read_usage(COUNT_LABEL, usage, USAGE)?;
    cache_writes::normalize(usage, &mut ir);
    if usage.is_some_and(|v| !v.is_null() && !v.is_object()) {
        ir.detail
            .refused_counts
            .push("Responses usage is not an unambiguous object".into());
    }

    ir.detail.usage_identity_note = (!crate::codec::usage_count::refused_any(usage, USAGE))
        .then(|| {
            crate::codec::usage_count::stated_total_note(
                COUNT_LABEL,
                RESPONSES_USAGE_IDENTITY,
                usage.and_then(|u| u.get(keys::TOTAL_TOKENS)),
                &mut ir,
            )
        })
        .flatten();
    tool_usage::read(response, &mut ir);
    ir.detail.service_tier = crate::codec::carry::read_word(
        map::WORDS_SERVED_TIER,
        response.and_then(|r| r.get(keys::SERVICE_TIER)),
    );
    Ok(ir)
}

/// OpenAI Responses streaming writer.
pub const UNSTAMPED_CREATED_AT: u64 = 0;

pub struct ResponsesWriter {
    /// Per-stream `sequence_number` counter, reset on the opening `MessageStart` and advanced once
    /// per event.
    sequence: AtomicU64,
    /// Whether the opening `MessageStart` was already written. The anthropic reader emits it 1:1
    /// with `message_start`, so a duplicate can arrive; an ungated reset would rewind
    /// `sequence_number`, mint a different `response.id` and clear the open-text set.
    started: AtomicBool,
    /// Whether `response.failed` was already written or local accumulation failed. A `MessageDelta` after it (Cohere/Gemini emit
    /// `Error` then `MessageDelta{Error}`) must write nothing, not a second terminal (RSP-11).
    failed: AtomicBool,
    /// A local accumulator refusal uses the existing encoding-error path with a truthful message.
    accumulation_failed: AtomicBool,
    /// Byte ceiling for any one per-item accumulator ([`Self::accum_byte_cap`]), set by the plane
    /// ([`crate::codec::proto_codec::ProtocolWriter::set_translate_cap`]); until then
    /// [`crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT`].
    translate_cap: AtomicUsize,
    /// All per-stream state behind one lock (`ResponsesStream`).
    stream: StreamCell<ResponsesStream>,
}

/// A Responses stream's per-stream state, behind the writer's one lock. A mid-stream
/// `Protocol::clone` is the same stream, so the clone carries all of it. A poisoned lock degrades
/// each field to its fallback instead of panicking on the request path.
#[derive(Clone)]
struct ResponsesStream {
    lifetime_blocks: usize,
    lifetime_payload_bytes: usize,
    finalized_bytes: usize,
    /// `response.id`, captured on the opening `MessageStart` and replayed on every later lifecycle
    /// event (`completed`/`incomplete`/`failed`), as native streams do. Without it the terminal
    /// event minted a fresh id on cross-protocol streams. Falls back to a fresh id.
    response_id: Option<String>,
    /// `response.created_at` (unix seconds), captured on the opening `MessageStart` and replayed on
    /// every later lifecycle event; differing timestamps across events are a proxy tell. Falls back
    /// to `now_unix_secs`.
    created_at: Option<u64>,
    /// `response.model`, captured on the opening `MessageStart` (after the DEFAULT_MODEL fallback)
    /// and replayed on every later lifecycle event, since `Response.model` is required and IR
    /// `MessageDelta`/`Error` carry none. Falls back to DEFAULT_MODEL.
    model: Option<String>,
    /// Output indices with a function-call `output_item.added`. `BlockStop` carries only the index,
    /// and a native stream emits `output_item.done` only for added items, so this stops a text
    /// block's `BlockStop` from emitting a spurious `function_call` done.
    open_tool_indices: std::collections::BTreeSet<usize>,
    /// Output indices with an open text message item (`output_item.added(message)` +
    /// `content_part.added`). A native stream always brackets a text part with the full lifecycle
    /// (`added`, `content_part.added`, `output_text.delta*`, `output_text.done`, `content_part.done`,
    /// `output_item.done`), since the SDK builds `output[]` from the added/done pair. Lets
    /// `BlockStop` emit the terminal frames for this index only.
    open_text_indices: std::collections::BTreeSet<usize>,
    /// IR-02: open text indices flagged `refusal` at `BlockStart`; their part is a `refusal` part
    /// from `content_part.added` to `output_item.done`.
    refusal_indices: std::collections::BTreeSet<usize>,
    /// Synthesized opaque `item_id`s keyed by `(kind-prefix, output_index)`. A native stream keeps a
    /// constant `item_id` across an item's lifecycle, so the id must be stable per key while still
    /// an opaque CSPRNG token. Falls back to a fresh id.
    item_ids: std::collections::BTreeMap<(&'static str, usize), String>,
    /// Dense native output positions for emitted items, keyed by the original IR index.
    /// Suppressed media does not allocate a position; closed items retain their mapping.
    wire_indices: std::collections::BTreeMap<usize, usize>,
    /// Function-call item fields by `output_index`. The native `output_item.done` carries the
    /// finalized item (`call_id`, `name`, full `arguments`), but `BlockStop` carries only the
    /// index, so `call_id` and `name` (from `BlockStart`) and `arguments` (concatenated
    /// `InputJsonDelta`s) are accumulated. Falls back to omitting them.
    tool_calls: std::collections::BTreeMap<usize, ToolCallAccum>,
    /// Streamed assistant text by `output_index`, concatenated from `TextDelta`s and drained into
    /// the terminal `output` message item at `BlockStop`. Falls back to empty text.
    text_accum: std::collections::BTreeMap<usize, String>,
    /// Citations by `output_index`. Responses carries them as `annotations` on the assembled
    /// `output_text` part, so a streamed `CitationsDelta` is held until `BlockStop` builds it.
    citation_accum: std::collections::BTreeMap<usize, CitationAccum>,
    /// Token logprobs by `output_index`. The IR streams them as a separate `LogprobsDelta`, but a
    /// native stream carries them on `output_text.done` and the finalized part, so they are held
    /// until `BlockStop`.
    logprob_accum: std::collections::BTreeMap<usize, LogprobAccum>,
    /// Finalized `output[]` items by `output_index`, recorded at each `BlockStop` and drained into
    /// the terminal `response.output` in index order (an empty `output` with nonzero
    /// `usage.output_tokens` is a shape real OpenAI never emits). Falls back to an empty array.
    output_items: std::collections::BTreeMap<usize, serde_json::Value>,
    /// Output indices with an open reasoning item, so the matching `BlockStop` emits a reasoning
    /// `output_item.done` and is not mistaken for a text/tool close.
    open_reasoning_indices: std::collections::BTreeSet<usize>,
    /// IR-17: open reasoning items whose Thinking block is a summary. Their text streams as
    /// `reasoning_summary_text` into `summary[]`; other reasoning items keep `content[]`.
    summary_reasoning_indices: std::collections::BTreeSet<usize>,
    /// Streamed reasoning text by `output_index` (from `ThinkingDelta`s), drained into the
    /// finalized reasoning item at `BlockStop`.
    reasoning_accum: std::collections::BTreeMap<usize, String>,
    /// Thinking signature by `output_index` (`IrDelta::SignatureDelta`). Responses has no signature
    /// frame; the blob is the finalized item's `encrypted_content` (RSP-01). Bounded like `reasoning_accum`.
    reasoning_sig_accum: std::collections::BTreeMap<usize, String>,
    /// The original ingress request body, captured via [`ProtocolWriter::set_request_echo`] before
    /// the first event. The spec requires every `Response` to mirror certain request members
    /// (`temperature`, `top_p`, `instructions`, `metadata`, `tool_choice`, `parallel_tool_calls`,
    /// `tools`). Streaming twin of `IrResponse::request_echo`. `None` when never set; the spec
    /// defaults then apply.
    request_echo: Option<serde_json::Value>,
}

/// Accumulated function-call item fields for one open `output_index`, finalized into the
/// `response.output_item.done` `item` object. `call_id`/`name` are captured from the opening
/// `BlockStart`; `arguments` is built by concatenating the streamed `InputJsonDelta` fragments.
#[derive(Clone, Default)]
struct ToolCallAccum {
    call_id: String,
    name: String,
    arguments: String,
}

/// Value-namespace constructor for [`ResponsesWriter`], so `Protocol::responses()` can write the
/// bare literal.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const ResponsesWriter: ResponsesWriter = ResponsesWriter {
    sequence: AtomicU64::new(0),
    started: AtomicBool::new(false),
    failed: AtomicBool::new(false),
    accumulation_failed: AtomicBool::new(false),
    translate_cap: AtomicUsize::new(crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT),
    stream: StreamCell::new(ResponsesStream::NEW),
};

impl ResponsesStream {
    /// A fresh stream: nothing captured, open or accumulated.
    const NEW: ResponsesStream = ResponsesStream {
        lifetime_blocks: 0,
        lifetime_payload_bytes: 0,
        finalized_bytes: 0,
        response_id: None,
        created_at: None,
        model: None,
        open_tool_indices: std::collections::BTreeSet::new(),
        open_text_indices: std::collections::BTreeSet::new(),
        refusal_indices: std::collections::BTreeSet::new(),
        item_ids: std::collections::BTreeMap::new(),
        wire_indices: std::collections::BTreeMap::new(),
        tool_calls: std::collections::BTreeMap::new(),
        text_accum: std::collections::BTreeMap::new(),
        citation_accum: std::collections::BTreeMap::new(),
        logprob_accum: std::collections::BTreeMap::new(),
        output_items: std::collections::BTreeMap::new(),
        open_reasoning_indices: std::collections::BTreeSet::new(),
        summary_reasoning_indices: std::collections::BTreeSet::new(),
        reasoning_accum: std::collections::BTreeMap::new(),
        reasoning_sig_accum: std::collections::BTreeMap::new(),
        request_echo: None,
    };
}

impl Clone for ResponsesWriter {
    /// A mid-stream clone is the same stream, so it carries the counter, latches, cap and cells.
    fn clone(&self) -> Self {
        ResponsesWriter {
            sequence: AtomicU64::new(self.sequence.load(Ordering::Relaxed)),
            started: AtomicBool::new(self.started.load(Ordering::Relaxed)),
            failed: AtomicBool::new(self.failed.load(Ordering::Relaxed)),
            accumulation_failed: AtomicBool::new(self.accumulation_failed.load(Ordering::Relaxed)),
            translate_cap: AtomicUsize::new(self.translate_cap.load(Ordering::Relaxed)),
            stream: self.stream.clone(),
        }
    }
}

/// Open `index` in `set` unless it is open already or the set holds `MAX_OPEN_TOOLS`.
fn open_bounded(set: &mut std::collections::BTreeSet<usize>, index: usize) -> bool {
    if set.contains(&index) || set.len() >= MAX_OPEN_TOOLS {
        return false;
    }
    set.insert(index)
}

impl ResponsesWriter {
    /// The stream's state, locked.
    fn stream(&self) -> std::sync::MutexGuard<'_, ResponsesStream> {
        self.stream.lock()
    }

    /// Byte ceiling for any one per-item accumulator. `MAX_OPEN_TOOLS` bounds how many items
    /// accumulate; this bounds how large one grows (a single open index could otherwise exhaust
    /// memory).
    fn accum_byte_cap(&self) -> usize {
        self.translate_cap.load(Ordering::Relaxed)
    }

    /// Latch the stream's translation cap (see [`Self::accum_byte_cap`]).
    fn store_translate_cap(&self, cap: usize) {
        self.translate_cap.store(cap, Ordering::Relaxed);
    }

    /// Reset `sequence_number` to 0 when the opening `response.created` is written, and clear the
    /// open-tool-index set so a reused writer carries no stale index into a fresh stream.
    fn reset_sequence_number(&self) {
        self.sequence.store(0, Ordering::Relaxed);

        self.failed.store(false, Ordering::Relaxed);
        self.accumulation_failed.store(false, Ordering::Relaxed);

        let mut stream = self.stream();
        let request_echo = stream.request_echo.take();
        *stream = ResponsesStream {
            request_echo,
            ..ResponsesStream::NEW
        };
    }

    /// Store the `response.id` captured on `MessageStart` for terminal events. A poisoned lock is a
    /// no-op (the terminal arm synthesizes a fresh id).
    fn set_response_id(&self, id: &str) {
        self.stream().response_id = Some(id.to_string());
    }

    /// The captured `response.id`, or `None` if never set (terminal event before `MessageStart`, or
    /// a poisoned lock); the caller then synthesizes one.
    fn carried_response_id(&self) -> Option<String> {
        self.stream().response_id.clone()
    }

    /// Store the `created_at` captured on `MessageStart` for terminal events. A poisoned lock is a
    /// no-op (falls back to `now_unix_secs`).
    fn set_created_at(&self, created_at: u64) {
        self.stream().created_at = Some(created_at);
    }

    /// The captured `created_at`, else the current unix time (never set, or poisoned lock). Replay
    /// keeps every event's `created_at` identical, as a native stream does.
    pub fn carried_created_at(&self, host: &dyn crate::codec::cells::CodecHost) -> u64 {
        let carried = self.stream().created_at;
        carried.unwrap_or_else(|| self.default_created_at(host))
    }

    /// The `created_at` for an answer that carries none: the host's wall clock
    /// (`CodecHost::wall_clock_now`, as the Chat writer stamps).
    pub(crate) fn default_created_at(&self, host: &dyn crate::codec::cells::CodecHost) -> u64 {
        host.wall_clock_now().unwrap_or(UNSTAMPED_CREATED_AT)
    }

    /// Store the `model` captured on `MessageStart` for terminal events. A poisoned lock is a no-op
    /// (falls back to `DEFAULT_MODEL`).
    fn set_model(&self, model: &str) {
        self.stream().model = Some(model.to_string());
    }

    /// The captured `model`, else `DEFAULT_MODEL` (never set, or poisoned lock). Replay keeps every
    /// event's `model` identical and non-null.
    fn carried_model(&self) -> String {
        self.stream()
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// Store the original ingress request body (see [`ProtocolWriter::set_request_echo`]). A
    /// poisoned lock is a no-op (echo members take the spec defaults).
    fn store_request_echo(&self, body: &serde_json::Value) {
        self.stream().request_echo = Some(body.clone());
    }

    /// The captured request echo, or `None` if never set. `fill_required_response_members` treats
    /// `None` like an empty object.
    fn carried_request_echo(&self) -> Option<serde_json::Value> {
        self.stream().request_echo.clone()
    }

    /// The next `sequence_number` (0 after [`Self::reset_sequence_number`]) and advance the counter.
    fn next_sequence_number(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    /// Open a function-call item at `index` so `BlockStop` knows to emit `output_item.done`.
    fn mark_tool_open(&self, index: usize) -> bool {
        open_bounded(&mut self.stream().open_tool_indices, index)
    }

    /// True and forget `index` if a function-call item was opened there; false for e.g. a text
    /// block, whose `BlockStop` must not emit `output_item.done` (also false on a poisoned lock).
    fn take_tool_open(&self, index: usize) -> bool {
        self.stream().open_tool_indices.remove(&index)
    }

    /// Record the `call_id`/`name` from the `BlockStart`'s `IrBlockMeta::ToolUse` so the
    /// `output_item.done` can emit the finalized item.
    fn record_tool_meta(&self, index: usize, call_id: &str, name: &str) {
        let mut stream = self.stream();
        let entry = stream.tool_calls.entry(index).or_default();
        entry.call_id = call_id.to_string();
        entry.name = name.to_string();
    }

    /// Append a streamed `arguments` fragment for the function-call item at `index`, since the
    /// native `output_item.done` carries the complete string.
    fn append_tool_arguments(&self, index: usize, fragment: &str) -> bool {
        let cap = self.accum_byte_cap();
        let map = &mut self.stream().tool_calls;
        match map.get_mut(&index) {
            Some(entry) => {
                if entry.arguments.len().saturating_add(fragment.len()) <= cap {
                    entry.arguments.push_str(fragment);
                } else {
                    return false;
                }
            }
            None => {
                if map.len() >= MAX_OPEN_TOOLS || fragment.len() > cap {
                    return false;
                }
                map.entry(index).or_default().arguments.push_str(fragment);
            }
        }
        true
    }

    /// Remove and return the accumulated function-call fields for `index`; `None` if nothing was
    /// accumulated (the `done` is then emitted without them).
    fn take_tool_accum(&self, index: usize) -> Option<ToolCallAccum> {
        self.stream().tool_calls.remove(&index)
    }

    /// Append a streamed text fragment for the message item at `index`, since the terminal
    /// `response.output` carries the complete text.
    fn append_text(&self, index: usize, fragment: &str) -> bool {
        let cap = self.accum_byte_cap();
        append_capped(&mut self.stream().text_accum, index, fragment, cap)
    }

    /// Buffer streamed citations for the message item at `index` until `BlockStop` assembles the
    /// annotated part.
    fn append_citations(&self, index: usize, cits: &[crate::codec::ir::IrCitation]) -> bool {
        if cits.is_empty() {
            return true;
        }
        let added = cits
            .iter()
            .map(citation_bytes)
            .fold(0usize, usize::saturating_add);
        let cap = self.accum_byte_cap();
        let map = &mut self.stream().citation_accum;
        match map.get_mut(&index) {
            Some(entry) => {
                let held = entry.bytes.saturating_add(added);
                if held <= cap {
                    entry.items.extend_from_slice(cits);
                    entry.bytes = held;
                } else {
                    return false;
                }
            }
            None => {
                if map.len() >= MAX_OPEN_TOOLS || added > cap {
                    return false;
                }
                map.insert(
                    index,
                    CitationAccum {
                        items: cits.to_vec(),
                        bytes: added,
                    },
                );
            }
        }
        true
    }

    /// Remove and return the accumulated citations for the message item at `index`.
    fn take_citation_accum(&self, index: usize) -> Vec<crate::codec::ir::IrCitation> {
        self.stream()
            .citation_accum
            .remove(&index)
            .map(|accum| accum.items)
            .unwrap_or_default()
    }

    /// Buffer streamed token logprobs for the message item at `index` until `BlockStop` builds
    /// `output_text.done` and the finalized part.
    fn append_logprobs(&self, index: usize, lps: &[crate::codec::ir::IrTokenLogprob]) -> bool {
        if lps.is_empty() {
            return true;
        }
        let added = lps
            .iter()
            .map(logprob_bytes)
            .fold(0usize, usize::saturating_add);
        let cap = self.accum_byte_cap();
        let map = &mut self.stream().logprob_accum;
        match map.get_mut(&index) {
            Some(entry) => {
                let held = entry.bytes.saturating_add(added);
                if held <= cap {
                    entry.items.extend_from_slice(lps);
                    entry.bytes = held;
                } else {
                    return false;
                }
            }
            None => {
                if map.len() >= MAX_OPEN_TOOLS || added > cap {
                    return false;
                }
                map.insert(
                    index,
                    LogprobAccum {
                        items: lps.to_vec(),
                        bytes: added,
                    },
                );
            }
        }
        true
    }

    /// Remove and return the accumulated token logprobs for the message item at `index`.
    fn take_logprob_accum(&self, index: usize) -> Vec<crate::codec::ir::IrTokenLogprob> {
        self.stream()
            .logprob_accum
            .remove(&index)
            .map(|accum| accum.items)
            .unwrap_or_default()
    }

    /// Remove and return the accumulated text for `index`, empty if none.
    fn take_text_accum(&self, index: usize) -> String {
        self.stream().text_accum.remove(&index).unwrap_or_default()
    }

    /// Record a finalized `output[]` item at `index` (as `BlockStop` assembles it) for the terminal
    /// event's `output` array.
    fn record_output_item(&self, index: usize, item: &serde_json::Value) -> Result<(), IrError> {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Count(0);
        self.require_accumulation(serde_json::to_writer(&mut bytes, item).is_ok())?;
        let mut stream = self.stream();
        let total = stream.finalized_bytes.saturating_add(bytes.0);
        self.require_accumulation(
            total <= self.accum_byte_cap()
                && (stream.output_items.contains_key(&index)
                    || stream.output_items.len() < crate::codec::ir::MAX_STREAM_BLOCKS),
        )?;
        let wire_index = stream.wire_indices.get(&index).copied().unwrap_or(index);
        stream.finalized_bytes = total;
        stream.output_items.insert(wire_index, item.clone());
        Ok(())
    }

    /// Reserve lifetime retained payload before any writer clones or accumulator allocations.
    /// Closed items remain charged because the terminal response retains their complete data.
    fn reserve_stream_event(&self, event: &IrStreamEvent) -> Result<(), IrError> {
        use crate::codec::ir::{IrBlockMeta, IrDelta};
        let (blocks, bytes) = match event {
            IrStreamEvent::BlockStart { block, .. } => (
                1,
                match block {
                    IrBlockMeta::ToolUse { id, name } => id.len().saturating_add(name.len()),
                    _ => 0,
                },
            ),
            IrStreamEvent::BlockDelta { delta, .. } => (
                0,
                match delta {
                    IrDelta::TextDelta(s)
                    | IrDelta::ThinkingDelta(s)
                    | IrDelta::InputJsonDelta(s)
                    | IrDelta::SignatureDelta(s)
                    | IrDelta::RedactedReasoningDelta(s) => s.len(),
                    IrDelta::CitationsDelta(c) => c
                        .iter()
                        .map(citation_bytes)
                        .fold(0usize, usize::saturating_add),
                    IrDelta::LogprobsDelta(l) => l
                        .iter()
                        .map(logprob_bytes)
                        .fold(0usize, usize::saturating_add),
                    IrDelta::MediaDelta(_) => 0,
                },
            ),
            _ => (0, 0),
        };
        let mut stream = self.stream();
        let blocks = stream.lifetime_blocks.saturating_add(blocks);
        let bytes = stream.lifetime_payload_bytes.saturating_add(bytes);
        self.require_accumulation(
            blocks <= crate::codec::ir::MAX_STREAM_BLOCKS && bytes <= self.accum_byte_cap(),
        )?;
        stream.lifetime_blocks = blocks;
        stream.lifetime_payload_bytes = bytes;
        Ok(())
    }

    /// Drain the finalized items in `output_index` order (the `BTreeMap` order), as a native stream
    /// assembles them.
    fn drain_output_items(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.stream().output_items)
            .into_values()
            .collect()
    }

    /// Mark a text message item open at `index` if it is not open and under the cap; true when this
    /// call opened it (so the caller emits `output_item.added`/`content_part.added` once).
    fn open_text_item(&self, index: usize) -> bool {
        open_bounded(&mut self.stream().open_text_indices, index)
    }

    /// IR-02: record that the text item at `index` is a refusal (its BlockStart said so).
    fn mark_refusal(&self, index: usize) {
        self.stream().refusal_indices.insert(index);
    }

    /// IR-02: whether the open text item at `index` is a refusal.
    fn is_refusal(&self, index: usize) -> bool {
        self.stream().refusal_indices.contains(&index)
    }

    /// IR-02: forget and return whether the text item at `index` was a refusal (at its BlockStop).
    fn take_refusal(&self, index: usize) -> bool {
        self.stream().refusal_indices.remove(&index)
    }

    /// True and forget `index` if a text item was open there (so `BlockStop` emits the text
    /// terminal frames for this index only).
    fn take_text_open(&self, index: usize) -> bool {
        self.stream().open_text_indices.remove(&index)
    }

    /// Mark a reasoning item open at `index` if not open and under the cap; true when this call
    /// opened it (so `output_item.added` is emitted once).
    fn open_reasoning_item(&self, index: usize) -> bool {
        open_bounded(&mut self.stream().open_reasoning_indices, index)
    }

    /// IR-17: remember that the reasoning item at `index` is a summary, or ask whether it is. A
    /// poisoned lock reads as "not a summary" (the `content[]` shape).
    fn mark_summary_reasoning(&self, index: usize) {
        self.stream().summary_reasoning_indices.insert(index);
    }

    fn is_summary_reasoning(&self, index: usize) -> bool {
        self.stream().summary_reasoning_indices.contains(&index)
    }

    fn take_summary_reasoning(&self, index: usize) -> bool {
        self.stream().summary_reasoning_indices.remove(&index)
    }

    /// True and forget `index` if a reasoning item was open there (so `BlockStop` emits the
    /// reasoning terminal frame for this index only).
    fn take_reasoning_open(&self, index: usize) -> bool {
        self.stream().open_reasoning_indices.remove(&index)
    }

    /// Append a streamed reasoning-text fragment for the item at `index`. Bounded as `append_text` is.
    fn append_reasoning(&self, index: usize, fragment: &str) -> bool {
        let cap = self.accum_byte_cap();
        append_capped(&mut self.stream().reasoning_accum, index, fragment, cap)
    }

    /// Remove and return the accumulated reasoning text for `index`, empty if none.
    fn take_reasoning_accum(&self, index: usize) -> String {
        self.stream()
            .reasoning_accum
            .remove(&index)
            .unwrap_or_default()
    }

    /// Buffer a streamed thinking-signature fragment until `BlockStop` writes it as the item's
    /// `encrypted_content` (RSP-01).
    fn append_reasoning_signature(&self, index: usize, fragment: &str) -> bool {
        if fragment.is_empty() {
            return true;
        }
        let cap = self.accum_byte_cap();
        append_capped(&mut self.stream().reasoning_sig_accum, index, fragment, cap)
    }

    /// Refuse an unrepresentable stream through the existing fallible encoding contract. Keeping
    /// this latch also stops direct writer callers from finalizing the retained prefix as success.
    fn require_accumulation(&self, accepted: bool) -> Result<(), IrError> {
        if accepted {
            return Ok(());
        }
        self.accumulation_failed.store(true, Ordering::Relaxed);
        self.failed.store(true, Ordering::Relaxed);
        Err(IrError {
            class: StatusClass::ServerError,
            retry_after: None,
            provider_signal: Some(ACCUMULATION_FAILURE_MSG.into()),
        })
    }

    /// Remove and return the buffered signature for `index`; `None` when the stream carried none
    /// (the item then has no `encrypted_content`).
    fn take_reasoning_signature(&self, index: usize) -> Option<String> {
        self.stream()
            .reasoning_sig_accum
            .remove(&index)
            .filter(|s| !s.is_empty())
    }

    /// Compact only emitted item starts; deltas and closes keep the mapping even after
    /// their IR block closes.
    fn compact_output_index(&self, event_type: &str, data: &mut serde_json::Value) {
        let Some(index) = data.get(OUTPUT_INDEX).and_then(|v| v.as_u64()) else {
            return;
        };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut stream = self.stream();
        if event_type == EVT_OUTPUT_ITEM_ADDED {
            let next = stream.wire_indices.len();
            stream.wire_indices.entry(index).or_insert(next);
        }
        if let Some(wire_index) = stream.wire_indices.get(&index) {
            data[OUTPUT_INDEX] = serde_json::json!(wire_index);
        }
    }

    /// The stream-stable opaque `item_id` for `(prefix, index)`, minted on first reference and then
    /// cached, so one item's `added`, `delta*` and `done` frames share an id the SDK correlates on
    /// while the id itself stays opaque.
    fn item_id_for(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        prefix: &'static str,
        index: usize,
    ) -> Result<String, IrError> {
        if let Some(id) = self.stream().item_ids.get(&(prefix, index)).cloned() {
            return Ok(id);
        }

        self.require_accumulation(
            self.stream().item_ids.len() < crate::codec::ir::MAX_STREAM_BLOCKS,
        )?;
        let minted = synthesize_item_id(host, prefix);
        let mut stream = self.stream();
        if let Some(id) = stream.item_ids.get(&(prefix, index)) {
            return Ok(id.clone());
        }
        let id = minted?;
        stream.item_ids.insert((prefix, index), id.clone());
        Ok(id)
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/input_hardening_tests.rs"]
mod input_hardening_tests;

#[cfg(test)]
#[path = "tests/field_carry_tests.rs"]
mod field_carry_tests;

#[cfg(test)]
#[path = "tests/float_usage_tests.rs"]
mod float_usage_tests;

#[cfg(test)]
#[path = "tests/annotation_stream_tests.rs"]
mod annotation_stream_tests;

#[cfg(test)]
#[path = "tests/ir_mapping_tests.rs"]
mod ir_mapping_tests;

#[cfg(test)]
#[path = "tests/ir_slot_wiring_tests.rs"]
mod ir_slot_wiring_tests;

#[cfg(test)]
#[path = "tests/ir_round3_tests.rs"]
mod ir_round3_tests;

#[cfg(test)]
#[path = "tests/usage_census_tests.rs"]
mod usage_census_tests;

#[cfg(test)]
#[path = "tests/df_map_audit_tests.rs"]
mod df_map_audit_tests;

const ACTION: &str = "action";
const SOURCES: &str = "sources";
const SEARCH: &str = "search";
const FILE_CITATION: &str = "file_citation";
const CONTAINER_FILE_CITATION: &str = "container_file_citation";
const FILE_PATH: &str = "file_path";

/// A `web_search_call` output item -> [`crate::codec::ir::IrBlock::HostedToolRecord`]: its `id`, its
/// `status`, and its search action's `sources[].url` as results (the query has no IR member).
fn read_web_search_call(item: &serde_json::Value) -> crate::codec::ir::IrBlock {
    let text = |k: &str| item.get(k).and_then(|v| v.as_str()).map(String::from);
    let results = item
        .get(ACTION)
        .and_then(|a| a.get(SOURCES))
        .and_then(|s| s.as_array())
        .map(|sources| {
            sources
                .iter()
                .filter_map(|src| {
                    Some(crate::codec::ir::IrSearchResult {
                        url: src.get(keys::URL)?.as_str()?.to_string(),
                        title: None,
                        snippet: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    crate::codec::ir::IrBlock::HostedToolRecord {
        kind: crate::codec::ir::IrHostedToolKind::WebSearch,
        call_id: text(keys::ID),
        status: text(keys::STATUS),
        results,
    }
}

/// A hosted web-search record -> a `web_search_call` output item (`id` synthesized when the record
/// carries none, `status` `completed` when it names none, its results as the search action's
/// `sources`).
fn write_web_search_call(
    host: &dyn crate::codec::cells::CodecHost,
    call_id: Option<&str>,
    status: Option<&str>,
    results: &[crate::codec::ir::IrSearchResult],
) -> Result<serde_json::Value, IrError> {
    let sources: Vec<serde_json::Value> = results
        .iter()
        .map(|r| serde_json::json!({ (keys::TYPE): keys::URL, (keys::URL): r.url }))
        .collect();
    Ok(serde_json::json!({
        (keys::TYPE): ITEM_TYPE_WEB_SEARCH_CALL,
        (keys::ID): match call_id { Some(id) => id.to_string(), None => synthesize_item_id(host, ITEM_ID_PREFIX_WEB_SEARCH)? },
        (keys::STATUS): status.unwrap_or(STATUS_COMPLETED),
        (ACTION): { (keys::TYPE): SEARCH, (SOURCES): sources },
    }))
}

/// The id prefix of a synthesized web-search item.
const ITEM_ID_PREFIX_WEB_SEARCH: &str = "ws";

/// The file annotations of an `output_text` part (`file_citation`, `container_file_citation`,
/// `file_path`) -> IR citations carrying [`crate::codec::ir::IrFileLocation`]; url citations are
/// read by `url_citation_wire`.
fn read_file_annotations(
    annotations: Option<&serde_json::Value>,
) -> Vec<crate::codec::ir::IrCitation> {
    let Some(items) = annotations.and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|a| {
            let kind = a.get(keys::TYPE)?.as_str()?;
            if ![FILE_CITATION, CONTAINER_FILE_CITATION, FILE_PATH].contains(&kind) {
                return None;
            }
            let text = |k: &str| a.get(k).and_then(|v| v.as_str()).map(String::from);
            Some(crate::codec::ir::IrCitation {
                kind: Some(kind.to_string()),
                file: Some(crate::codec::ir::IrFileLocation {
                    file_id: text(keys::FILE_ID),
                    filename: text(keys::FILENAME),
                    index: a.get(keys::INDEX).and_then(|v| v.as_i64()),
                }),
                raw: Some(a.clone()),
                ..Default::default()
            })
        })
        .collect()
}

/// The IR file citations of a text block as Responses `file_citation` annotations (a citation that
/// arrived as a container or path annotation keeps its own kind, from `raw`).
fn file_annotations(citations: &[crate::codec::ir::IrCitation]) -> Vec<serde_json::Value> {
    citations
        .iter()
        .filter_map(|c| {
            let f = c.file.as_ref()?;
            if let Some(raw) = &c.raw {
                return Some(raw.clone());
            }
            let mut a = serde_json::Map::new();
            a.insert(keys::TYPE.to_string(), serde_json::json!(FILE_CITATION));
            if let Some(id) = &f.file_id {
                a.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
            }
            if let Some(name) = &f.filename {
                a.insert(keys::FILENAME.to_string(), serde_json::json!(name));
            }
            a.insert(
                keys::INDEX.to_string(),
                serde_json::json!(f.index.unwrap_or(0)),
            );
            Some(serde_json::Value::Object(a))
        })
        .collect()
}

const MODERATION: &str = "moderation";
const MODERATION_INPUT: &str = "input";
const MODERATION_OUTPUT: &str = "output";
const MODERATION_CATEGORIES: &str = "categories";

/// A response's `moderation.{input,output}` (each a `moderation_result`) -> the IR's safety verdicts
/// one flagged verdict per category set `true`; scores do not cross.
fn read_moderation(body: &serde_json::Value) -> Vec<crate::codec::ir::IrSafetyVerdict> {
    [MODERATION_INPUT, MODERATION_OUTPUT]
        .into_iter()
        .flat_map(|side| {
            crate::codec::ir::IrSafetyVerdict::flagged_categories(
                body.get(MODERATION)
                    .and_then(|m| m.get(side))
                    .and_then(|r| r.get(MODERATION_CATEGORIES)),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "tests/audit_cache_usage_tests.rs"]
mod audit_cache_usage_tests;

#[cfg(test)]
#[path = "tests/audit_part_indices_tests.rs"]
mod audit_part_indices_tests;

#[cfg(test)]
#[path = "tests/audit_stream_lifetime_tests.rs"]
mod audit_stream_lifetime_tests;

#[cfg(test)]
#[path = "tests/audit_completion_tests.rs"]
mod audit_completion_tests;
