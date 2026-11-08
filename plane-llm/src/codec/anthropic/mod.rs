// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The Anthropic (Messages API) dialect: this file is the declaration, `reader.rs` is wire to IR,
//! `writer.rs` is IR to wire, `handler.rs` names the operations it serves, and `tests/` holds its
//! own tests.

pub mod handler;
#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
mod reader;
mod writer;

mod auth;
mod blocks;
mod citations;
mod ids;
mod slots;
mod usage;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;
use blocks::*;
use citations::*;
use ids::*;
pub use ids::{request_id_from_entropy, synth_anthropic_request_id};
use slots::*;
use usage::*;

#[cfg(test)]
use crate::codec::ir::IrUsage;
use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};
use crate::codec::protocol::*;
use busbar_contract::http::StatusCode;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;

#[allow(unused_imports)]
use super::proto_codec::*;
use crate::codec::egress_prep::LaneCaps;

#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

/// Build this dialect's wire codec (a fresh instance per resolution).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, AnthropicReader, AnthropicWriter)
}

/// A native Anthropic stream emits `event: ping` right after `message_start`; a translated stream
/// never did, which is a fingerprintable proxy tell.
const ANTHROPIC_PING_SSE_FRAME: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";

/// Anthropic's `GET /v1/models` envelope: `model` objects in a single complete page
/// (`data`, `has_more: false`, `first_id`, `last_id`).
fn models_list_envelope(names: &[&str]) -> serde_json::Value {
    let data: Vec<serde_json::Value> = names
        .iter()
        .map(|id| {
            serde_json::json!({
                (keys::TYPE): keys::MODEL,
                (keys::ID): id,
                "display_name": id,
                "created_at": "1970-01-01T00:00:00Z",
                "capabilities": null,
                "max_input_tokens": null,
                "max_tokens": null
            })
        })
        .collect();
    serde_json::json!({
        (keys::DATA): data,
        "has_more": false,
        "first_id": names.first(),
        "last_id": names.last(),
    })
}

/// Router detection: the `anthropic-version`/`anthropic-beta` headers (2), the `x-api-key`
fn claims(
    h: crate::codec::protocol::HeadFields<'_>,
    path: &str,
) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::framework::head_field;
    use crate::codec::protocol::ClaimStrength;
    if head_field(h, HDR_ANTHROPIC_VERSION).is_some() || head_field(h, "anthropic-beta").is_some() {
        return Some(ClaimStrength(2));
    }
    if head_field(h, HDR_X_API_KEY).is_some() {
        return Some(ClaimStrength(4));
    }
    if path.contains("/v1/messages") {
        return Some(ClaimStrength(11));
    }
    None
}

/// Headerless detection: a `/v1/messages` path (exact or model-prefixed) names Anthropic (40).
fn residual_claims(path: &str) -> Option<crate::codec::protocol::ClaimStrength> {
    if path == "/v1/messages" || path.ends_with("/v1/messages") {
        return Some(crate::codec::protocol::ClaimStrength(40));
    }
    None
}

pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::AnthropicRequestHandler),
    verbs: &[busbar_contract::operation::OpVerb::CHAT],
    #[cfg(test)]
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    streaming_content_type: Some(crate::codec::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: None,

    native_tool_id_prefix: Some("toolu_"),
    #[cfg(test)]
    ingress_auth: IngressAuth::Bearer,

    #[cfg(test)]
    egress_auth_headers: None,
    #[cfg(test)]
    egress_auth_lane_constant: false,
    #[cfg(test)]
    egress_scheme: Some(EgressScheme::Static {
        families: &[
            CredentialFamily {
                prefix: CRED_PREFIX_API_KEY,
                presented_as: CredentialHeader::Raw {
                    header: HDR_X_API_KEY,
                    trim_start: true,
                },
            },
            CredentialFamily {
                prefix: CRED_PREFIX_OAUTH,
                presented_as: CredentialHeader::Bearer,
            },
        ],
        own: CredentialHeader::Raw {
            header: HDR_X_API_KEY,
            trim_start: false,
        },
        passthrough: CredentialHeader::Bearer,
    }),
    stream_usage_requires_opt_in: false,
    requires_max_tokens: true,
    #[cfg(test)]
    stop_sequence_cap: None,
    cache_markers_model_gated: false,
    fills_thought_signature: false,
    frame_after_message_start: Some(ANTHROPIC_PING_SSE_FRAME),
    max_cache_control_breakpoints: Some(4),
    #[cfg(test)]
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: false,
    max_citations_per_delta: Some(1),

    egress_user_agent: "Anthropic/Python 0.39.0",
    has_model_in_url: false,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::UNAUTHORIZED.as_u16(),
        crate::codec::protocol::ERR_TYPE_AUTHENTICATION,
    ),
    ingress_relayed_response_header_names: &[HDR_REQUEST_ID],
    auth_failure_message: "invalid x-api-key",
    has_native_path_not_found: false,
    egress_stream_accept: crate::codec::protocol::TEXT_EVENT_STREAM,
    models_list_envelope: Some(models_list_envelope),
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: None,

    list_models_fingerprint_headers: &[HDR_ANTHROPIC_VERSION],

    static_headers: &[(HDR_ANTHROPIC_VERSION, ANTHROPIC_API_VERSION)],
};

/// This dialect's registration (listed in `crate::codec::DIALECTS`).
pub(crate) const ENTRY: super::proto_codec::DialectEntry = super::proto_codec::DialectEntry {
    decl: &DECL,
    protocol,
    with_writer: |f| {
        let w = AnthropicWriter;
        f(&w)
    },
    with_reader: |f| f(&AnthropicReader),
    leaf: &crate::codec::leaf_codec::LeafCodecs::NONE,
};

/// Value of the required `anthropic-version` request header.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Mixed-case base62 alphabet, uppercase-first, matching a native Anthropic id token (`01` plus a
/// mixed-case alphanumeric token, not lowercase hex).
const ANTHROPIC_NATIVE_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The response header a native Anthropic endpoint always carries (the SDK reads it into
/// `request_id`).
const HDR_REQUEST_ID: &str = "request-id";

/// Width of a synthesized id's token after the `01` marker (a native `msg_`/`req_` id is
/// prefix + `01` + 24 mixed-case base62 chars); matching length and alphabet keeps it
/// indistinguishable from a native one.
const SYNTH_ID_TOKEN_LEN: usize = 24;

/// SSE event-type strings emitted in the `event:` header of each native Anthropic stream frame.
const EVT_MESSAGE_START: &str = "message_start";
const EVT_CONTENT_BLOCK_START: &str = "content_block_start";
const EVT_CONTENT_BLOCK_DELTA: &str = "content_block_delta";
const EVT_CONTENT_BLOCK_STOP: &str = "content_block_stop";
const EVT_MESSAGE_DELTA: &str = "message_delta";
const EVT_MESSAGE_STOP: &str = "message_stop";

/// `content_block_delta` sub-type values (`delta.type` field).
const DELTA_TYPE_TEXT: &str = "text_delta";
const DELTA_TYPE_THINKING: &str = "thinking_delta";
const DELTA_TYPE_INPUT_JSON: &str = "input_json_delta";
const DELTA_TYPE_SIGNATURE: &str = "signature_delta";
const DELTA_TYPE_CITATIONS: &str = "citations_delta";

/// Native Anthropic `stop_reason` token values.
const STOP_END_TURN: &str = "end_turn";
const STOP_MAX_TOKENS: &str = keys::MAX_TOKENS;
const STOP_STOP_SEQUENCE: &str = keys::STOP_SEQUENCE;
const STOP_TOOL_USE: &str = "tool_use";
const STOP_PAUSE_TURN: &str = "pause_turn";
const STOP_REFUSAL: &str = "refusal";

/// `stop_reason` reported when the CONTEXT WINDOW (not the caller's `max_tokens`) ran out. The
/// reader maps it to [`crate::codec::ir::IrStopReason::MaxTokens`] (every writer renders `Other` as
/// a natural stop, which would tell a foreign client a truncated answer was complete) and sets
/// [`crate::codec::ir::IrStopDetail::ContextWindowExceeded`] beside it, which this writer (and
/// Bedrock's) renders back as this token.
const STOP_MODEL_CONTEXT_WINDOW_EXCEEDED: &str = "model_context_window_exceeded";

/// `output_config.format.type` for native structured outputs; the slot a cross-protocol
/// `response_format` projects into (the deprecated beta spelling is top-level `output_format`).
const OUTPUT_FORMAT_JSON_SCHEMA: &str = "json_schema";

/// `thinking.type` for adaptive thinking, the only on-mode Opus 4.7+/5.x, Sonnet 5 and Fable
/// accept (`{type:"enabled",budget_tokens}` 400s there).
const THINKING_TYPE_ADAPTIVE: &str = "adaptive";

/// `thinking.type` that switches reasoning off ([`crate::codec::ir::IrReasoningAsk::Off`]).
const THINKING_TYPE_DISABLED: &str = "disabled";

/// Tool `type` for an ordinary client (function) tool; absent `type` means the same. Any other
/// `type` is an Anthropic-defined server or client-executed tool whose schema is Anthropic's.
const TOOL_TYPE_CUSTOM: &str = "custom";

/// Content block `type` values not covered by the delta sub-type constants above.
const BLOCK_TYPE_REDACTED_THINKING: &str = "redacted_thinking";

/// `extra` key parking the raw native content blocks the IR cannot model (e.g. `document`), by
/// position in `req.messages` (system-role messages excluded).
const ANTHROPIC_UNMODELED_BLOCKS_SENTINEL: &str = "__busbar_anthropic_unmodeled_blocks";

/// Every content-block kind [`read_block`] reads (the request and answer grammar, `codec::drops`).
const BLOCK_KINDS: &[&str] = &[
    keys::TEXT,
    keys::THINKING,
    STOP_TOOL_USE,
    TOOL_RESULT,
    keys::IMAGE,
    BLOCK_TYPE_DOCUMENT,
    BLOCK_TYPE_SEARCH_RESULT,
    BLOCK_TYPE_REDACTED_THINKING,
    WEB_SEARCH_TOOL_RESULT,
];

/// The request content-block grammar: the turns' blocks and the system blocks.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[
    crate::codec::drops::Blocks {
        at: &["messages[]", "content[]"],
        tag: Some(keys::TYPE),
        modelled: BLOCK_KINDS,
        companions: &[],
    },
    crate::codec::drops::Blocks {
        at: &["system[]"],
        tag: Some(keys::TYPE),
        modelled: BLOCK_KINDS,
        companions: &[],
    },
];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, "type=text"),
    (crate::codec::drops::kind::IMAGE, "type=image"),
    (crate::codec::drops::kind::DOCUMENT, "type=document"),
    (crate::codec::drops::kind::THINKING, "type=thinking"),
    (crate::codec::drops::kind::TOOL_USE, "type=tool_use"),
    (crate::codec::drops::kind::TOOL_RESULT, "type=tool_result"),
];

/// The IR request members the reader carries by code from a path no map-file row names (how a drop
/// of one is named by the caller's wire path).
const REQUEST_CODE_NAMES: &[(&str, &str)] = &[
    (
        crate::codec::drops::name::PARALLEL_TOOL_CALLS,
        "tool_choice.disable_parallel_tool_use",
    ),
    (
        crate::codec::drops::name::RESPONSE_FORMAT,
        "output_config.format",
    ),
];

/// The answer content-block grammar.
const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["content[]"],
    tag: Some(keys::TYPE),
    modelled: BLOCK_KINDS,
    companions: &[],
}];

/// What the Anthropic reader parks in `extra` beside the members its map file does not model.
const PARKED: &[crate::codec::drops::Parked] = &[
    crate::codec::drops::Parked {
        key: ANTHROPIC_UNMODELED_BLOCKS_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::THINKING,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::OUTPUT_FORMAT,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::OUTPUT_CONFIG,
        holds: crate::codec::drops::Holds::Members(&[keys::EFFORT, keys::FORMAT]),
    },
    crate::codec::drops::Parked {
        key: keys::METADATA,
        holds: crate::codec::drops::Holds::Members(&[USER_ID]),
    },
];

/// What this dialect's answers carry beyond its map file's rows. `stop_details` (the refusal
/// category) is read beside the stop reason; the stream's block index is structure; `ping` and
/// `error` are carried by code.
const RESPONSE_CODE: &[&str] = &[STOP_DETAILS];
const STREAM_CODE: &[&str] = &["type=content_block_delta.index", "type=ping", "type=error"];

/// The answer paths INSIDE a subtree this dialect carries that its code does not carry, named by the
/// drop walk.
const RESPONSE_DROPS: &[&str] = &[
    "content[].type=bash_code_execution_tool_result",
    "content[].type=code_execution_tool_result",
    "content[].type=container_upload",
    "content[].type=server_tool_use.caller.type=code_execution_20250825.tool_id",
    "content[].type=server_tool_use.caller.type=code_execution_20260120",
    "content[].type=text_editor_code_execution_tool_result",
    "content[].type=tool_search_tool_result",
    "content[].type=tool_use.caller.type=code_execution_20250825.tool_id",
    "content[].type=tool_use.caller.type=code_execution_20260120",
    "content[].type=tool_use.toolset_name",
    "content[].type=web_fetch_tool_result",
    "content[].type=web_search_tool_result.caller.type=code_execution_20250825.tool_id",
    "content[].type=web_search_tool_result.caller.type=code_execution_20260120",
    "content[].type=web_search_tool_result.content[].encrypted_content",
    "content[].type=web_search_tool_result.content[].page_age",
];
const STREAM_DROPS: &[&str] = &[
    "type=content_block_start.content_block.type=bash_code_execution_tool_result",
    "type=content_block_start.content_block.type=code_execution_tool_result",
    "type=content_block_start.content_block.type=container_upload",
    "type=content_block_start.content_block.type=server_tool_use.caller.type=code_execution_20250825.tool_id",
    "type=content_block_start.content_block.type=server_tool_use.caller.type=code_execution_20260120",
    "type=content_block_start.content_block.type=text_editor_code_execution_tool_result",
    "type=content_block_start.content_block.type=tool_search_tool_result",
    "type=content_block_start.content_block.type=tool_use.caller.type=code_execution_20250825.tool_id",
    "type=content_block_start.content_block.type=tool_use.caller.type=code_execution_20260120",
    "type=content_block_start.content_block.type=tool_use.toolset_name",
    "type=content_block_start.content_block.type=web_fetch_tool_result",
    "type=content_block_start.content_block.type=web_search_tool_result",
    "type=message_delta.delta.container.expires_at",
    "type=message_delta.delta.container.skills",
];

/// The block `type` values [`read_block`] holds whole in the IR; any other is parked under
/// [`ANTHROPIC_UNMODELED_BLOCKS_SENTINEL`].
fn is_modeled_anthropic_block_type(t: &str) -> bool {
    matches!(
        t,
        keys::TEXT
            | keys::THINKING
            | STOP_TOOL_USE
            | TOOL_RESULT
            | keys::IMAGE
            | BLOCK_TYPE_DOCUMENT
            | BLOCK_TYPE_REDACTED_THINKING
    )
}

/// The block `type` values the stream reader translates; a `content_block_start` of any other
/// type is suppressed with its deltas and stop.
fn is_streamed_anthropic_block_type(t: &str) -> bool {
    matches!(
        t,
        keys::TEXT | keys::THINKING | STOP_TOOL_USE | keys::IMAGE | BLOCK_TYPE_REDACTED_THINKING
    )
}

/// The `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`] this protocol produces: a
/// Files-API `{"type":"file","file_id":…}` document source or a `{"type":"content"}` document.
const VENDOR_NAME: &str = "anthropic";

/// Native `document` content block type (a PDF/text attachment).
const BLOCK_TYPE_DOCUMENT: &str = "document";

/// The one mime an Anthropic `text` document source carries.
const DOCUMENT_MIME_TEXT_PLAIN: &str = "text/plain";

/// The one mime an Anthropic `base64` document source accepts.
const DOCUMENT_MIME_PDF: &str = keys::APPLICATION_PDF;

#[cfg(test)]
include!("tests/mod_document_preparations_helper.rs");

/// `document.source` for inline bytes, by mime: a PDF rides the `base64` source; `text/*` rides the
/// `text` source as decoded text (`media_type: "text/plain"`) since the IR carries it base64.
fn inline_document_source(media_type: &str, data: &str) -> Option<serde_json::Value> {
    #[cfg(test)]
    DOCUMENT_PREPARATIONS.with(|count| count.set(count.get() + 1));
    if media_type.eq_ignore_ascii_case(DOCUMENT_MIME_PDF) {
        return Some(serde_json::json!({
            (keys::TYPE): keys::BASE64,
            (MEDIA_TYPE): DOCUMENT_MIME_PDF,
            (keys::DATA): data,
        }));
    }
    let is_text = media_type
        .split('/')
        .next()
        .is_some_and(|top| top.eq_ignore_ascii_case(keys::TEXT));
    if is_text {
        let bytes = busbar_contract::media::base64_decode(data)?;
        let text = String::from_utf8(bytes.to_vec()).ok()?;
        return Some(serde_json::json!({
            (keys::TYPE): keys::TEXT,
            (MEDIA_TYPE): DOCUMENT_MIME_TEXT_PLAIN,
            (keys::DATA): text,
        }));
    }
    None
}

/// Native `search_result` block type (a caller-supplied RAG passage). Read into a `Text` block
/// carrying an [`crate::codec::ir::IrCitation`]; deliberately not in
/// [`is_modeled_anthropic_block_type`] (see [`read_block`]).
const BLOCK_TYPE_SEARCH_RESULT: &str = "search_result";

/// The stash entry member marking a raw block the IR does not hold at all (an insertion, not a
/// replacement, on a same-dialect write).
const STASH_INSERT: &str = "insert";

/// Scan a message's raw `content` array (before `read_blocks` parses it) for blocks the IR does
/// not hold whole, pushing `{"m","i","block"}` entries (`i` is the raw index).
fn stash_unmodeled_blocks(
    msg_val: &serde_json::Value,
    m: usize,
    sink: &mut Vec<serde_json::Value>,
) {
    let Some(content_arr) = msg_val.get(keys::CONTENT).and_then(|c| c.as_array()) else {
        return;
    };
    for (i, block_val) in content_arr.iter().enumerate() {
        let block_type = block_val
            .get(keys::TYPE)
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let document_needs_stash = block_type == BLOCK_TYPE_DOCUMENT
            && (block_val.get(keys::CONTEXT).is_some() || block_val.get(keys::CITATIONS).is_some());
        if !is_read_into_ir(block_val) {
            sink.push(serde_json::json!({
                (keys::M): m, (keys::I): i, (keys::BLOCK): block_val, (STASH_INSERT): true
            }));
        } else if !is_modeled_anthropic_block_type(block_type) || document_needs_stash {
            sink.push(serde_json::json!({ (keys::M): m, (keys::I): i, (keys::BLOCK): block_val }));
        }
    }
}

/// Borrow the native stash once per request; duplicate replacements keep the first valid entry.
#[derive(Default)]
struct MessageStash<'a> {
    replacements: std::collections::BTreeMap<usize, &'a serde_json::Value>,
    inserts: Vec<(usize, &'a serde_json::Value)>,
}

fn index_stash(
    sentinel: &[serde_json::Value],
) -> std::collections::BTreeMap<usize, MessageStash<'_>> {
    let mut messages = std::collections::BTreeMap::<usize, MessageStash<'_>>::new();
    for entry in sentinel {
        let Some(m) = entry
            .get(keys::M)
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
        else {
            continue;
        };
        let Some(i) = entry
            .get(keys::I)
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
        else {
            continue;
        };
        let Some(block) = entry.get(keys::BLOCK) else {
            continue;
        };
        let message = messages.entry(m).or_default();
        if entry.get(STASH_INSERT).and_then(|v| v.as_bool()) == Some(true) {
            message.inserts.push((i, block));
        } else {
            message.replacements.entry(i).or_insert(block);
        }
    }
    for message in messages.values_mut() {
        message.inserts.sort_by_key(|(i, _)| *i);
    }
    messages
}

/// Error `type` strings for error envelopes and in-stream error events. Shared values alias
/// their canonical home; only `timeout_error` is Anthropic-specific.
const ERR_TYPE_OVERLOADED: &str = crate::codec::framework::ERR_TYPE_OVERLOADED;
const ERR_TYPE_INVALID_REQUEST: &str = crate::codec::protocol::ERR_TYPE_INVALID_REQUEST;
const ERR_TYPE_AUTHENTICATION: &str = crate::codec::protocol::ERR_TYPE_AUTHENTICATION;
const ERR_TYPE_RATE_LIMIT: &str = crate::codec::protocol::ERR_TYPE_RATE_LIMIT;
const ERR_TYPE_API_ERROR: &str = crate::codec::protocol::ERR_TYPE_API_ERROR;
const ERR_TYPE_TIMEOUT: &str = "timeout_error";
/// The billing `ErrorResponse.error` type. No cross-dialect alias exists (OpenAI names the same
/// condition `insufficient_quota`).
const ERR_TYPE_BILLING: &str = "billing_error";
const ERR_TYPE_NOT_FOUND: &str = crate::codec::protocol::ERR_TYPE_NOT_FOUND;
const ERR_TYPE_PERMISSION: &str = crate::codec::protocol::ERR_TYPE_PERMISSION;
const ERR_TYPE_REQUEST_TOO_LARGE: &str = crate::codec::protocol::ERR_TYPE_REQUEST_TOO_LARGE;

const BATCH: &str = "batch";
const BLOCKED_DOMAINS: &str = "blocked_domains";
const CACHE_CONTROL: &str = "cache_control";
const CACHE_CREATION: &str = "cache_creation";
const CACHE_CREATION_INPUT_TOKENS: &str = "cache_creation_input_tokens";
const CACHE_READ_INPUT_TOKENS: &str = "cache_read_input_tokens";
const W_CALLER: &str = "caller";
const CATEGORY: &str = "category";
const CITED_TEXT: &str = "cited_text";
const CONTENT_BLOCK: &str = "content_block";
const DIRECT: &str = "direct";
const DISABLE_PARALLEL_TOOL_USE: &str = "disable_parallel_tool_use";
const DOCUMENT_INDEX: &str = "document_index";
const DOCUMENT_TITLE: &str = "document_title";
const ENCRYPTED_INDEX: &str = "encrypted_index";
const END_BLOCK_INDEX: &str = "end_block_index";
const END_CHAR_INDEX: &str = "end_char_index";
const END_PAGE_NUMBER: &str = "end_page_number";
const EPHEMERAL_1H_INPUT_TOKENS: &str = "ephemeral_1h_input_tokens";
const EPHEMERAL_5M_INPUT_TOKENS: &str = "ephemeral_5m_input_tokens";
const EXPLANATION: &str = "explanation";
const IS_ERROR: &str = "is_error";
const MAX_USES: &str = "max_uses";
const MEDIA_TYPE: &str = "media_type";
const PARTIAL_JSON: &str = "partial_json";
const PRIORITY: &str = "priority";
const REQUEST_ID: &str = "request_id";
const SEARCH_RESULT_INDEX: &str = "search_result_index";
const SERVER_TOOL_USE: &str = "server_tool_use";
const STANDARD: &str = "standard";
const START_BLOCK_INDEX: &str = "start_block_index";
const START_CHAR_INDEX: &str = "start_char_index";
const START_PAGE_NUMBER: &str = "start_page_number";
const STOP_DETAILS: &str = "stop_details";
const STOP_REASON: &str = "stop_reason";
const THINKING_TOKENS: &str = "thinking_tokens";
const TOOL_RESULT: &str = "tool_result";
const TOOL_USE_ID: &str = "tool_use_id";
const USER_ID: &str = "user_id";
const WEB_SEARCH_REQUESTS: &str = "web_search_requests";
/// `usage.server_tool_use.web_fetch_requests`.
const WEB_FETCH_REQUESTS: &str = "web_fetch_requests";

/// Citation `type` tag values.
const CITATION_TYPE_CHAR: &str = "char_location";
const CITATION_TYPE_PAGE: &str = "page_location";
const CITATION_TYPE_CONTENT_BLOCK: &str = "content_block_location";
const CITATION_TYPE_WEB_SEARCH: &str = "web_search_result_location";
const CITATION_TYPE_SEARCH_RESULT: &str = keys::SEARCH_RESULT_LOCATION;

/// The sole valid `cache_control.type` Anthropic exposes today.
const CACHE_KIND_EPHEMERAL: &str = "ephemeral";

/// Header names used when attaching Anthropic credentials to upstream requests.
const HDR_X_API_KEY: &str = "x-api-key";
const HDR_ANTHROPIC_VERSION: &str = "anthropic-version";

/// Credential prefix strings used to classify a raw key into its native Anthropic scheme.
#[cfg(test)]
const CRED_PREFIX_API_KEY: &str = "sk-ant-api";
#[cfg(test)]
const CRED_PREFIX_OAUTH: &str = "sk-ant-oat";

/// The upstream path this writer targets on the Anthropic Messages API.
const PATH_UPSTREAM: &str = "/v1/messages";

/// HTTP status codes that Anthropic (and cross-protocol upstreams) use to signal overload.
const STATUS_OVERLOADED: u16 = 503;
const STATUS_ANTHROPIC_OVERLOADED: u16 = 529;

/// Upper bound on an upstream-supplied streaming block index. The index flows into the IR and then
/// into writers that allocate against it, so a hostile backend sending up to `u64::MAX` could drive
/// a pathological allocation.
const MAX_ANTHROPIC_BLOCK_INDEX: u64 = 1023;

/// A streaming event's `index`, bounded by `MAX_ANTHROPIC_BLOCK_INDEX`. `None` (the event is
/// dropped) when it is missing or non-numeric; unlike the Bedrock reader, which defaults to 0.
fn read_clamped_block_index(data: &serde_json::Value) -> Option<usize> {
    data.get(keys::INDEX)
        .and_then(|i| i.as_u64())
        .filter(|index| *index <= MAX_ANTHROPIC_BLOCK_INDEX)
        .and_then(|index| usize::try_from(index).ok())
}

#[derive(Clone)]
pub struct AnthropicReader;

/// Map a streaming `error.type` token to its breaker `StatusClass`, so a mid-stream error drives
/// the same breaker transition an equivalent HTTP error would (`overloaded_error`, `rate_limit_error`,
/// `api_error` and `timeout_error` are transient; authentication, permission and billing are hard
/// down; the caller-fault types do not penalize the lane).
fn stream_error_class(error_type: Option<&str>) -> StatusClass {
    match error_type {
        Some(ERR_TYPE_OVERLOADED) => StatusClass::Overloaded,
        Some(ERR_TYPE_RATE_LIMIT) => StatusClass::RateLimit,
        Some(ERR_TYPE_API_ERROR) => StatusClass::ServerError,
        Some(ERR_TYPE_TIMEOUT) => StatusClass::Timeout,
        Some(ERR_TYPE_AUTHENTICATION) | Some(ERR_TYPE_PERMISSION) => StatusClass::Auth,
        Some(ERR_TYPE_BILLING) => StatusClass::Billing,
        Some(ERR_TYPE_INVALID_REQUEST)
        | Some(ERR_TYPE_NOT_FOUND)
        | Some(ERR_TYPE_REQUEST_TOO_LARGE)
        | None => StatusClass::ClientError,
        Some(_unrecognized) => StatusClass::ClientError,
    }
}

/// A provider-run tool's record from another dialect has no Anthropic form: a
/// `web_search_tool_result` must carry each result's `encrypted_content`, which only Anthropic
/// mints.
fn warn_hosted_record_dropped() {
    crate::codec::drops::writer_drop!(
        crate::codec::drops::HOSTED_TOOL,
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        [],
        "dropping a hosted web-search record on Anthropic egress: a web_search_tool_result needs \
         the encrypted_content only Anthropic mints (lossy-by-target)"
    );
}

/// [`write_block`] plus the members the published response block schemas require and a request
/// block must not carry: `text` gets `citations: null`, `tool_use` gets `caller: {"type":"direct"}`,
/// `thinking` gets `signature: ""` (matching the `content_block_start` seed).
fn write_response_block(block: &crate::codec::ir::IrBlock) -> serde_json::Value {
    let mut val = write_block_in_context(block, true);
    if let Some(obj) = val.as_object_mut() {
        match obj.get(keys::TYPE).and_then(|t| t.as_str()) {
            Some(keys::TEXT) => {
                obj.entry(keys::CITATIONS)
                    .or_insert(serde_json::Value::Null);
            }
            Some(STOP_TOOL_USE) => {
                obj.entry(W_CALLER)
                    .or_insert_with(|| serde_json::json!({ (keys::TYPE): DIRECT }));
            }
            Some(keys::THINKING) => {
                obj.entry(keys::SIGNATURE)
                    .or_insert_with(|| serde_json::json!(""));
            }
            _ => {}
        }
    }
    val
}

/// Parse `cache_control` (`{"type":"ephemeral"}`) into the IR's `CacheControl`. Shared by every
/// site that can carry a breakpoint (text/system blocks, tool definitions, tool_use/tool_result)
/// so none is silently dropped.
fn read_cache_control(
    val: Option<&serde_json::Value>,
) -> Result<Option<crate::codec::ir::CacheControl>, IrError> {
    let Some(cc_val) = val else { return Ok(None) };
    if cc_val.is_null() {
        return Ok(None);
    }
    let Some(cc_obj) = cc_val.as_object() else {
        return Err(ir_parse_error());
    };
    match cc_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
        Some(CACHE_KIND_EPHEMERAL) => Ok(Some(crate::codec::ir::CacheControl {
            kind: crate::codec::ir::CacheKind::Ephemeral,
        })),
        None => Err(ir_parse_error()),
        Some(_) => Err(ir_parse_error()),
    }
}

fn write_cache_control(cc: &crate::codec::ir::CacheControl) -> serde_json::Value {
    match cc.kind {
        crate::codec::ir::CacheKind::Ephemeral => {
            serde_json::json!({(keys::TYPE): CACHE_KIND_EPHEMERAL})
        }
    }
}

/// Normalize `tool_choice` (`{"type":"auto"|"any"|"tool"|"none","name"?}`) into the IR union:
fn read_anthropic_tool_choice(
    val: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    let obj = val?.as_object()?;
    match obj.get(keys::TYPE).and_then(|t| t.as_str())? {
        keys::AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
        keys::NONE_WORD => Some(crate::codec::ir::IrToolChoice::None),
        keys::ANY => Some(crate::codec::ir::IrToolChoice::Required),
        keys::TOOL => obj
            .get(keys::NAME)
            .and_then(|n| n.as_str())
            .filter(|name| !name.is_empty())
            .map(|name| crate::codec::ir::IrToolChoice::Tool {
                name: name.to_string(),
            }),
        _ => None,
    }
}

fn write_anthropic_tool_choice(tc: &crate::codec::ir::IrToolChoice) -> serde_json::Value {
    match tc {
        crate::codec::ir::IrToolChoice::Auto => serde_json::json!({(keys::TYPE): keys::AUTO}),
        crate::codec::ir::IrToolChoice::None => serde_json::json!({(keys::TYPE): keys::NONE_WORD}),
        crate::codec::ir::IrToolChoice::Required => serde_json::json!({(keys::TYPE): keys::ANY}),
        crate::codec::ir::IrToolChoice::Tool { name } => {
            serde_json::json!({(keys::TYPE): keys::TOOL, (keys::NAME): name})
        }
    }
}

/// Anthropic `stop_reason` to [`crate::codec::ir::IrStopReason`]; an unmodeled token is `Other`.
fn read_anthropic_stop_reason(token: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match token {
        STOP_END_TURN => S::EndTurn,
        STOP_MAX_TOKENS => S::MaxTokens,
        STOP_STOP_SEQUENCE => S::StopSequence,
        STOP_TOOL_USE => S::ToolUse,
        STOP_PAUSE_TURN => S::PauseTurn,
        STOP_REFUSAL => S::Refusal,
        STOP_MODEL_CONTEXT_WINDOW_EXCEEDED => S::MaxTokens,
        _ => S::Other,
    }
}

/// [`crate::codec::ir::IrStopReason`] to Anthropic `stop_reason`. Anthropic has no `safety` member:
fn write_anthropic_stop_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::EndTurn => STOP_END_TURN,
        S::MaxTokens => STOP_MAX_TOKENS,
        S::StopSequence => STOP_STOP_SEQUENCE,
        S::ToolUse => STOP_TOOL_USE,
        S::PauseTurn => STOP_PAUSE_TURN,
        S::Refusal | S::Safety => STOP_REFUSAL,
        S::Error | S::Other => STOP_END_TURN,
    }
}

/// The refinement of a stop: `model_context_window_exceeded` becomes
/// [`crate::codec::ir::IrStopDetail::ContextWindowExceeded`] beside the coarse `MaxTokens`, and a
/// `refusal` stop's `stop_details` becomes [`crate::codec::ir::IrStopDetail::Refusal`].
fn read_anthropic_stop_detail(
    stop_reason: Option<&str>,
    stop_details: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrStopDetail> {
    match stop_reason? {
        STOP_MODEL_CONTEXT_WINDOW_EXCEEDED => {
            Some(crate::codec::ir::IrStopDetail::ContextWindowExceeded)
        }
        STOP_REFUSAL => {
            let d = stop_details?.as_object()?;
            if d.get(keys::TYPE).and_then(|t| t.as_str()) != Some(STOP_REFUSAL) {
                return None;
            }
            let text = |k: &str| d.get(k).and_then(|v| v.as_str()).map(String::from);
            Some(crate::codec::ir::IrStopDetail::Refusal {
                category: text(CATEGORY),
                explanation: text(EXPLANATION),
            })
        }
        _ => None,
    }
}

/// The `stop_reason` token for a coarse reason plus its refinement: a length stop refined as
/// ContextWindowExceeded is `model_context_window_exceeded`; everything else is
/// [`write_anthropic_stop_reason`].
fn write_anthropic_stop_reason_detailed(
    reason: crate::codec::ir::IrStopReason,
    detail: Option<&crate::codec::ir::IrStopDetail>,
) -> &'static str {
    match (reason, detail) {
        (
            crate::codec::ir::IrStopReason::MaxTokens,
            Some(crate::codec::ir::IrStopDetail::ContextWindowExceeded),
        ) => STOP_MODEL_CONTEXT_WINDOW_EXCEEDED,
        _ => write_anthropic_stop_reason(reason),
    }
}

/// The `stop_details` member (required, nullable): the refusal object when the stop is written as
/// `refusal` and the IR carries a Refusal detail, else `null`.
fn write_anthropic_stop_details(
    reason: Option<crate::codec::ir::IrStopReason>,
    detail: Option<&crate::codec::ir::IrStopDetail>,
) -> serde_json::Value {
    match (reason.map(write_anthropic_stop_reason), detail) {
        (
            Some(STOP_REFUSAL),
            Some(crate::codec::ir::IrStopDetail::Refusal {
                category,
                explanation,
            }),
        ) => serde_json::json!({
            (keys::TYPE): STOP_REFUSAL,
            (CATEGORY): category,
            (EXPLANATION): explanation,
        }),
        _ => serde_json::Value::Null,
    }
}

fn write_block(block: &crate::codec::ir::IrBlock) -> serde_json::Value {
    write_block_in_context(block, false)
}

fn write_block_in_context(block: &crate::codec::ir::IrBlock, response: bool) -> serde_json::Value {
    write_block_prepared(block, response, None)
}

fn write_block_prepared(
    block: &crate::codec::ir::IrBlock,
    response: bool,
    prepared_document: Option<serde_json::Value>,
) -> serde_json::Value {
    if let Some(sr) = block.as_search_result() {
        return write_search_result(&sr, block);
    }
    match block {
        crate::codec::ir::IrBlock::Text {
            text,
            cache_control,
            citations,
            refusal: _,
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::TEXT));
            obj.insert(keys::TEXT.to_string(), serde_json::json!(text));
            if let Some(cc) = cache_control {
                let cc_val = match cc.kind {
                    crate::codec::ir::CacheKind::Ephemeral => {
                        serde_json::json!({(keys::TYPE): CACHE_KIND_EPHEMERAL})
                    }
                };
                obj.insert(CACHE_CONTROL.to_string(), cc_val);
            }
            if !citations.is_empty() {
                let arr: Vec<serde_json::Value> = citations
                    .iter()
                    .filter_map(|citation| {
                        if response {
                            write_response_citation(citation)
                        } else {
                            write_citation(citation)
                        }
                    })
                    .collect();
                if !arr.is_empty() {
                    obj.insert(keys::CITATIONS.to_string(), serde_json::Value::Array(arr));
                }
            }
            serde_json::Value::Object(obj)
        }

        crate::codec::ir::IrBlock::Thinking {
            text,
            redacted: true,
            cache_control,
            ..
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                keys::TYPE.to_string(),
                serde_json::json!(BLOCK_TYPE_REDACTED_THINKING),
            );
            obj.insert(keys::DATA.to_string(), serde_json::json!(text));
            if let Some(cc) = cache_control {
                obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
            }
            serde_json::Value::Object(obj)
        }
        crate::codec::ir::IrBlock::Thinking {
            text,
            signature,
            redacted: false,
            cache_control,
            kind: _,
            signature_origin: _,
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::THINKING));
            obj.insert(keys::THINKING.to_string(), serde_json::json!(text));
            if let Some(sig) = signature {
                obj.insert(keys::SIGNATURE.to_string(), serde_json::json!(sig));
            }
            if let Some(cc) = cache_control {
                obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
            }
            serde_json::Value::Object(obj)
        }
        crate::codec::ir::IrBlock::ToolUse {
            id,
            name,
            input,
            cache_control,

            thought_signature: _,
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert(keys::TYPE.to_string(), serde_json::json!(STOP_TOOL_USE));
            obj.insert(keys::ID.to_string(), serde_json::json!(id));
            obj.insert(keys::NAME.to_string(), serde_json::json!(name));
            obj.insert(keys::INPUT.to_string(), input.clone());
            if let Some(cc) = cache_control {
                obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
            }
            serde_json::Value::Object(obj)
        }
        crate::codec::ir::IrBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert(keys::TYPE.to_string(), serde_json::json!(TOOL_RESULT));
            obj.insert(TOOL_USE_ID.to_string(), serde_json::json!(tool_use_id));
            if content.is_empty() {
                obj.insert(keys::CONTENT.to_string(), serde_json::json!(""));
            } else {
                let kept: Vec<serde_json::Value> = content
                    .iter()
                    .enumerate()
                    .filter_map(|(index, b)| crate::codec::drops::with_child_origin(index, || {
                        let mut prepared_document = None;
                        if !attachment_is_sendable(b, &mut prepared_document)
                            || matches!(b, crate::codec::ir::IrBlock::Text { text, .. } if text.is_empty()) {
                            return None;
                        }
                        Some(match b {
                        crate::codec::ir::IrBlock::Json(v) => serde_json::json!({
                            (keys::TYPE): keys::TEXT,
                            (keys::TEXT): serde_json::to_string(v).unwrap_or_default(),
                        }),
                        other => write_block_prepared(other, false, prepared_document),
                        })
                    }))
                    .collect();
                if kept.is_empty() {
                    obj.insert(keys::CONTENT.to_string(), serde_json::json!(""));
                } else {
                    obj.insert(keys::CONTENT.to_string(), serde_json::Value::Array(kept));
                }
            }
            if *is_error {
                obj.insert(IS_ERROR.to_string(), serde_json::Value::Bool(true));
            }
            if let Some(cc) = cache_control {
                obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
            }
            serde_json::Value::Object(obj)
        }
        crate::codec::ir::IrBlock::Image {
            source,
            cache_control,
            detail: _,
        } => {
            let mut img = match source {
                crate::codec::ir::IrImageSource::Url(url)
                | crate::codec::ir::IrImageSource::UrlWithMime { url, .. } => {
                    serde_json::json!({ (keys::TYPE): keys::IMAGE, (keys::SOURCE): { (keys::TYPE): keys::URL, (keys::URL): url } })
                }
                crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                    match crate::codec::ir::image_subtype_if_supported(media_type) {
                        Some(subtype) => serde_json::json!({
                            (keys::TYPE): keys::IMAGE,
                            (keys::SOURCE): { (keys::TYPE): keys::BASE64, (MEDIA_TYPE): format!("image/{subtype}"), (keys::DATA): data }
                        }),
                        None => {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::IMAGE,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [media_type = %media_type, ],
                                "dropping image block on Anthropic egress: media_type is not one of \
                                 image/{{jpeg,png,gif,webp}}, the only set Anthropic accepts — \
                                 emitting it verbatim would 400 the backend");
                            serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" })
                        }
                    }
                }

                crate::codec::ir::IrImageSource::Vendor { vendor, value }
                    if *vendor == VENDOR_NAME =>
                {
                    serde_json::json!({ (keys::TYPE): keys::IMAGE, (keys::SOURCE): value })
                }
                crate::codec::ir::IrImageSource::Vendor { .. } => {
                    serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" })
                }
            };
            if let Some(cc) = cache_control {
                if let Some(obj) = img.as_object_mut() {
                    obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
                }
            }
            img
        }
        crate::codec::ir::IrBlock::Media {
            kind,
            source,
            name,
            cache_control,
            citations,
            context,
        } => {
            if *kind != crate::codec::ir::IrMediaKind::Document {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::block(kind.as_str()),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [media_kind = kind.as_str(),],
                    "dropping attachment on Anthropic egress: the Messages API has a `document` \
                     content block and NO audio or video block, so this attachment has no native \
                     slot; it is NOT emitted"
                );
                return serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" });
            }
            let src = match source {
                crate::codec::ir::IrImageSource::Url(url)
                | crate::codec::ir::IrImageSource::UrlWithMime { url, .. } => {
                    serde_json::json!({ (keys::TYPE): keys::URL, (keys::URL): url })
                }
                crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                    match prepared_document.or_else(|| inline_document_source(media_type, data)) {
                        Some(src) => src,
                        None => {
                            return serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" });
                        }
                    }
                }

                crate::codec::ir::IrImageSource::Vendor { value, .. } => value.clone(),
            };
            let mut doc =
                serde_json::json!({ (keys::TYPE): BLOCK_TYPE_DOCUMENT, (keys::SOURCE): src });
            if let Some(obj) = doc.as_object_mut() {
                if let Some(n) = name {
                    obj.insert(keys::TITLE.to_string(), serde_json::json!(n));
                }

                if let Some(c) = context {
                    obj.insert(keys::CONTEXT.to_string(), serde_json::json!(c));
                }
                if let Some(enabled) = citations {
                    obj.insert(
                        keys::CITATIONS.to_string(),
                        serde_json::json!({ (keys::ENABLED): enabled }),
                    );
                }
                if let Some(cc) = cache_control {
                    obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
                }
            }
            doc
        }
        crate::codec::ir::IrBlock::Json(_) | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
            serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" })
        }
    }
}

/// IR effort word to `output_config.effort`. There is no `minimal`; `low` is the nearest.
fn anthropic_effort_word(effort: crate::codec::ir::IrReasoningEffort) -> &'static str {
    match effort {
        crate::codec::ir::IrReasoningEffort::Minimal => keys::LOW,
        other => other.as_str(),
    }
}

/// `output_config.effort` word to IR effort. `xhigh`/`max` sit above `High`; each foreign writer
/// projects them onto its own top and `anthropic_effort_word` writes them back verbatim.
fn read_anthropic_effort_word(word: &str) -> Option<crate::codec::ir::IrReasoningEffort> {
    crate::codec::ir::IrReasoningEffort::parse_extended(word)
        .filter(|e| *e != crate::codec::ir::IrReasoningEffort::Minimal)
}

/// `output_config.format` (or the deprecated `output_format`) to the typed IR directive. Only the
/// `json_schema` form with an object schema is one; anything else yields `None` (the key still
/// rides `extra` same-protocol).
fn read_anthropic_output_format(
    v: &serde_json::Value,
) -> Option<crate::codec::ir::IrResponseFormat> {
    if v.get(keys::TYPE).and_then(|t| t.as_str()) != Some(OUTPUT_FORMAT_JSON_SCHEMA) {
        return None;
    }
    let schema = v.get(keys::SCHEMA).filter(|s| s.is_object())?.clone();
    Some(crate::codec::ir::IrResponseFormat::bare(true, Some(schema)))
}

/// Close every object schema (`additionalProperties: false`) in place. Anthropic rejects a
/// structured-output schema without it, while OpenAI accepts an open object in non-strict mode.
fn close_object_schemas(schema: &mut serde_json::Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let is_object = obj.contains_key(keys::PROPERTIES)
        || match obj.get(keys::TYPE) {
            Some(serde_json::Value::String(t)) => t == keys::OBJECT,
            Some(serde_json::Value::Array(ts)) => ts.iter().any(|t| t == keys::OBJECT),
            _ => false,
        };
    if is_object && obj.get(keys::ADDITIONAL_PROPERTIES) != Some(&serde_json::Value::Bool(false)) {
        obj.insert(
            keys::ADDITIONAL_PROPERTIES.to_string(),
            serde_json::Value::Bool(false),
        );
    }
    for key in [keys::PROPERTIES, "$defs", "definitions"] {
        if let Some(map) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
            map.values_mut().for_each(close_object_schemas);
        }
    }
    for key in ["anyOf", "allOf", "oneOf", "prefixItems"] {
        if let Some(arr) = obj.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.iter_mut().for_each(close_object_schemas);
        }
    }
    for key in ["items", "not"] {
        if let Some(sub) = obj.get_mut(key) {
            close_object_schemas(sub);
        }
    }
}

/// Request-side attachment filter shared by a message's content and a `tool_result`'s content.
fn attachment_is_sendable(
    block: &crate::codec::ir::IrBlock,
    prepared_document: &mut Option<serde_json::Value>,
) -> bool {
    match block {
        crate::codec::ir::IrBlock::Image { source, .. } => match source {
            crate::codec::ir::IrImageSource::Vendor { vendor, .. } => {
                if *vendor == VENDOR_NAME {
                    return true;
                }
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::IMAGE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [vendor = %vendor, ],
                    "dropping unresolvable vendor-scoped image reference on Anthropic egress: a \
                     Responses input_image.file_id or a Bedrock s3Location has no cross-vendor analog; \
                     the block is NOT emitted");
                false
            }
            crate::codec::ir::IrImageSource::Base64 { media_type, .. } => {
                if crate::codec::ir::image_subtype_if_supported(media_type).is_some() {
                    return true;
                }
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::IMAGE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [media_type = %media_type, ],
                    "dropping image block from anthropic request egress: media_type is not one of \
                     image/{{jpeg,png,gif,webp}} and anthropic 400s anything else");
                false
            }
            crate::codec::ir::IrImageSource::Url(_)
            | crate::codec::ir::IrImageSource::UrlWithMime { .. } => true,
        },
        crate::codec::ir::IrBlock::Media { kind, source, .. } => {
            if *kind != crate::codec::ir::IrMediaKind::Document {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::block(kind.as_str()),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [media_kind = kind.as_str(),],
                    "dropping attachment from anthropic request egress: the Messages API has no \
                     audio or video content block"
                );
                return false;
            }
            match source {
                crate::codec::ir::IrImageSource::Vendor { vendor, .. }
                    if *vendor != VENDOR_NAME =>
                {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::DOCUMENT,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [vendor = %vendor, ],
                        "dropping document attachment from anthropic request egress: the source is \
                         a foreign vendor file handle anthropic's backend cannot resolve");
                    false
                }
                crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                    *prepared_document = inline_document_source(media_type, data);
                    if prepared_document.is_some() {
                        return true;
                    }
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::DOCUMENT,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [media_type = %media_type, ],
                        "dropping document attachment from anthropic request egress: anthropic has \
                         an inline document source only for application/pdf (base64) and UTF-8 \
                         text (text source); this mime has no native slot");
                    false
                }
                _ => true,
            }
        }
        _ => true,
    }
}

/// A native `search_result` block: `source`, `title`, the passage as one text part, the caller's
/// `citations` switch and the cache breakpoint.
fn write_search_result(
    sr: &crate::codec::ir::IrSearchResultParts<'_>,
    block: &crate::codec::ir::IrBlock,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        keys::TYPE.to_string(),
        serde_json::json!(BLOCK_TYPE_SEARCH_RESULT),
    );
    obj.insert(keys::SOURCE.to_string(), serde_json::json!(sr.source));
    obj.insert(keys::TITLE.to_string(), serde_json::json!(sr.title));
    obj.insert(
        keys::CONTENT.to_string(),
        serde_json::json!([{ (keys::TYPE): keys::TEXT, (keys::TEXT): sr.body }]),
    );
    if let Some(cfg) = sr.citations_config {
        obj.insert(keys::CITATIONS.to_string(), cfg.clone());
    }
    if let crate::codec::ir::IrBlock::Text {
        cache_control: Some(cc),
        ..
    } = block
    {
        obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
    }
    serde_json::Value::Object(obj)
}

fn write_message(
    msg: &crate::codec::ir::IrMessage,
    stash: Option<&MessageStash<'_>>,
    origins: &[crate::codec::ir::IrBlockOrigin],
) -> serde_json::Value {
    let role_str = match msg.role {
        crate::codec::ir::IrRole::User => keys::USER,
        crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,

        crate::codec::ir::IrRole::System => keys::USER,

        crate::codec::ir::IrRole::Tool => keys::USER,
    };

    let mut dropped_unsigned_thinking = 0usize;

    let mut inserts = stash.into_iter().flat_map(|s| s.inserts.iter()).peekable();
    let mut raw = 0usize;
    let mut blocks: Vec<serde_json::Value> = Vec::with_capacity(msg.content.len());
    for (block_index, block) in msg.content.iter().enumerate() {
        while inserts.peek().is_some_and(|(at, _)| *at <= raw) {
            if let Some((_, b)) = inserts.next() {
                blocks.push((*b).clone());
            }
            raw += 1;
        }
        let i = raw;
        raw += 1;
        let written: Option<serde_json::Value> = crate::codec::drops::with_block_origin(
            origins.get(block_index),
            || 'block: {
                if let crate::codec::ir::IrBlock::Thinking {
                    signature,
                    redacted: false,
                    signature_origin,
                    ..
                } = block
                {
                    let foreign = signature_origin
                        .is_some_and(|o| o != crate::codec::ir::IrSignatureOrigin::Anthropic);
                    if signature.is_none() || foreign {
                        dropped_unsigned_thinking += 1;
                        if crate::codec::drops::is_open() {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::THINKING,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [],
                                "dropping an assistant thinking block with no Anthropic signature"
                            );
                        }
                        break 'block None;
                    }
                }
                let mut prepared_document = None;
                if !attachment_is_sendable(block, &mut prepared_document) {
                    break 'block None;
                }
                if block.is_citation_carrier() {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::TEXT,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "dropping citations with no text on Anthropic egress: an empty text block is \
                     rejected (COH-17)"
                    );
                    break 'block None;
                }

                if let Some(original) = stash.and_then(|s| s.replacements.get(&i)) {
                    break 'block Some((*original).clone());
                }
                if let crate::codec::ir::IrBlock::HostedToolRecord { .. } = block {
                    warn_hosted_record_dropped();
                    break 'block None;
                }
                Some(write_block_prepared(block, false, prepared_document))
            },
        );
        blocks.extend(written);
    }
    blocks.extend(inserts.map(|(_, b)| (*b).clone()));
    if dropped_unsigned_thinking > 0 && !crate::codec::drops::is_open() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::THINKING,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [dropped = dropped_unsigned_thinking,],
            "dropped assistant thinking block(s) with no Anthropic signature (none, or another \
             vendor's, IR-18) from anthropic request egress (anthropic rejects them with a 400)"
        );
    }

    let content_val: serde_json::Value = serde_json::Value::Array(blocks);
    serde_json::json!({ (keys::ROLE): role_str, (keys::CONTENT): content_val })
}

/// Project one IR tool onto `tools[]`. `None` for a hosted tool that is not Anthropic-defined (a
/// Responses `{"type":"web_search"}` has no `name`); as a function tool it would 400.
fn write_tool(tool: &crate::codec::ir::IrTool) -> Option<serde_json::Value> {
    if let Some(hosted) = &tool.hosted {
        let anthropic_shaped = hosted
            .get(keys::TYPE)
            .and_then(|t| t.as_str())
            .is_some_and(|t| t != TOOL_TYPE_CUSTOM)
            && hosted.get(keys::NAME).and_then(|n| n.as_str()).is_some();
        if anthropic_shaped {
            return Some(hosted.clone());
        }
        crate::codec::drops::writer_drop!(
            crate::codec::drops::TOOLS,
            &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
            [],
            "dropping a hosted tool on Anthropic egress: it is not an Anthropic-defined tool and has \
             no Anthropic projection"
        );
        return None;
    }
    let mut obj = serde_json::Map::new();
    obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));
    if let Some(desc) = &tool.description {
        obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
    }
    obj.insert(keys::INPUT_SCHEMA.to_string(), tool.input_schema.clone());
    if let Some(cc) = &tool.cache_control {
        obj.insert(CACHE_CONTROL.to_string(), write_cache_control(cc));
    }

    if let Some(strict) = tool.strict {
        obj.insert(keys::STRICT.to_string(), serde_json::json!(strict));
    }
    Some(serde_json::Value::Object(obj))
}

/// Anthropic writer implementation.
///
/// `open_block_indices` is the per-stream set of IR block indices whose `content_block_start` this
/// writer wrote, and which owe a `content_block_stop`; an index is added only where the frame is
/// written. `redacted_thinking` defers its start to the delta carrying the opaque bytes, so
/// marking it at `BlockStart` could close a block the client never saw opened. `IrBlockMeta::Image`
/// is never tracked: the response wire has no `image` content block, so it projects to no frame.
/// The `BlockStop` arm carries only an index, so this set is how it tells a suppressed index from
/// an opened one. Mirrors `BedrockWriter`'s guard.
#[derive(Clone)]
pub struct AnthropicWriter {
    stream: StreamCell<AnthropicStream>,
}

/// An Anthropic stream's per-stream state (see [`AnthropicWriter`]).
#[derive(Clone)]
struct AnthropicStream {
    wire_indices: std::collections::BTreeMap<usize, usize>,
    open_block_indices: std::collections::BTreeSet<usize>,
    tool_input_started: std::collections::BTreeSet<usize>,
    invalid_tool_input: bool,
    /// This stream's `message.id`, minted once and replayed on every later `message_start`. The
    /// Anthropic reader emits `MessageStart` 1:1 with the upstream frame, so a duplicate can reach
    /// this writer; a native id is fixed for the life of the message, so the first one wins.
    message_id: Option<String>,
}

/// Value-namespace constructor for [`AnthropicWriter`]. `protocol_for` builds a fresh writer per
/// stream, so each use inlines an independent empty set.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const AnthropicWriter: AnthropicWriter = AnthropicWriter {
    stream: StreamCell::new(AnthropicStream {
        wire_indices: std::collections::BTreeMap::new(),
        open_block_indices: std::collections::BTreeSet::new(),
        tool_input_started: std::collections::BTreeSet::new(),
        invalid_tool_input: false,
        message_id: None,
    }),
};

#[cfg(test)]
include!("tests/mod_anthropic_writer_helper.rs");

impl AnthropicWriter {
    /// Only blocks with an Anthropic projection consume a native content index.
    fn compact_frames(
        &self,
        mut frames: Vec<(String, serde_json::Value)>,
    ) -> Vec<(String, serde_json::Value)> {
        let mut stream = self.stream.lock();
        for (event, frame) in &mut frames {
            let Some(index) = frame
                .get(keys::INDEX)
                .and_then(serde_json::Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
            else {
                continue;
            };
            if event == EVT_CONTENT_BLOCK_START && !stream.wire_indices.contains_key(&index) {
                let next = stream.wire_indices.len();
                stream.wire_indices.insert(index, next);
            }
            if let Some(wire_index) = stream.wire_indices.get(&index) {
                frame[keys::INDEX] = serde_json::json!(wire_index);
            }
        }
        frames
    }
    /// THE STREAM'S `message.id`: the first one wins.
    fn carried_message_id(
        &self,
        mint: impl FnOnce() -> Result<String, IrError>,
    ) -> Result<String, IrError> {
        let mut stream = self.stream.lock();
        if let Some(id) = &stream.message_id {
            return Ok(id.clone());
        }
        let id = mint()?;
        stream.message_id = Some(id.clone());
        Ok(id)
    }

    /// Record that a `content_block_start` was WRITTEN for IR block `index`, so it owes a closing
    /// `content_block_stop`.
    fn mark_block_open(&self, index: usize) -> bool {
        self.stream.lock().open_block_indices.insert(index)
    }

    /// Consume the open record for `index`, returning whether this writer opened it (and so owes
    /// the closing frame).
    fn take_block_open(&self, index: usize) -> bool {
        self.stream.lock().open_block_indices.remove(&index)
    }
}

#[cfg(test)]
#[path = "tests/anthropic_hardening_tests.rs"]
mod anthropic_hardening_tests;

#[cfg(test)]
#[path = "tests/input_hardening_tests.rs"]
mod input_hardening_tests;

#[cfg(test)]
#[path = "tests/user_and_parallelism_carry_tests.rs"]
mod user_and_parallelism_carry_tests;

#[cfg(test)]
#[path = "tests/reasoning_carry_tests.rs"]
mod reasoning_carry_tests;

#[cfg(test)]
#[path = "tests/field_carry_tests.rs"]
mod field_carry_tests;

#[cfg(test)]
#[path = "tests/usage_float_tests.rs"]
mod usage_float_tests;

#[cfg(test)]
#[path = "tests/usage_census_tests.rs"]
mod usage_census_tests;

#[cfg(test)]
#[path = "tests/ir_mapping_q57_tests.rs"]
mod ir_mapping_q57_tests;

#[cfg(test)]
#[path = "tests/ir_slot_wiring_tests.rs"]
mod ir_slot_wiring_tests;

#[cfg(test)]
#[path = "tests/ir_round3_tests.rs"]
mod ir_round3_tests;

#[cfg(test)]
#[path = "tests/hosted_record_tests.rs"]
mod hosted_record_tests;

#[cfg(test)]
#[path = "tests/stream_error_member_tests.rs"]
mod stream_error_member_tests;

#[cfg(test)]
#[path = "tests/wire_representability_tests.rs"]
mod wire_representability_tests;

#[cfg(test)]
#[path = "tests/parser_refusal_tests.rs"]
mod parser_refusal_tests;

#[cfg(test)]
#[path = "tests/audit_usage_tests.rs"]
mod audit_usage_tests;

#[cfg(test)]
#[path = "tests/performance_regression_tests.rs"]
mod performance_regression_tests;
