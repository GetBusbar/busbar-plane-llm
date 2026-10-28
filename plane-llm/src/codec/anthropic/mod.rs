// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The Anthropic (Messages API) dialect: this file is the declaration, `reader.rs` is wire to IR,
//! `writer.rs` is IR to wire, `handler.rs` names the operations it serves, and `tests/` holds its
//! own tests. The `busbar` binary links the crate and hands [`DECL`] to core's
//! `proto::registry::install_protocols` at boot; core never names this dialect.
//!
//! Core's test/`test-support` builds compile these same sources back in as `proto::anthropic`, so
//! every core reference here is spelled with core's crate name and every self reference is relative.

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
use busbar_contract::http::StatusCode;
use busbar_contract::protocol::*;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;
// Reached relatively so it resolves both standalone and netted into core; redundant with the
// `busbar_contract::protocol::*` glob when netted.
#[allow(unused_imports)]
use super::proto_codec::*;
use busbar_contract::ir::egress_prep::LaneCaps;
// Named explicitly: a `test-support` build of busbar-core re-exports a second copy of these items
// through the `busbar_contract::protocol::*` glob, and a bare use would be ambiguous.
#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

/// Build this dialect's wire codec (a fresh instance per resolution).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, AnthropicReader, AnthropicWriter)
}

/// A native Anthropic stream emits `event: ping` right after `message_start`; a translated stream
/// never did, which is a fingerprintable proxy tell. Injected once after the translated
/// `message_start` on any ingress-Anthropic cross-protocol stream (same-protocol passthrough is
/// byte-verbatim and already carries the upstream's pings).
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
                "created_at": "1970-01-01T00:00:00Z"
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
/// header, Anthropic's alone among the six (4, catches callers who omit the version header), then
/// the `/v1/messages` path (11). Lower strength binds tighter.
fn claims(
    h: busbar_contract::protocol::HeadFields<'_>,
    path: &str,
) -> Option<busbar_contract::protocol::ClaimStrength> {
    use busbar_contract::protocol::{head_field, ClaimStrength};
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
fn residual_claims(path: &str) -> Option<busbar_contract::protocol::ClaimStrength> {
    if path == "/v1/messages" || path.ends_with("/v1/messages") {
        return Some(busbar_contract::protocol::ClaimStrength(40));
    }
    None
}

pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::AnthropicRequestHandler),
    verbs: &[busbar_contract::operation::OpVerb::CHAT],
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    streaming_content_type: Some(busbar_contract::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: None,
    // `toolu_…` is Anthropic's documented native tool-call id shape.
    native_tool_id_prefix: Some("toolu_"),
    ingress_auth: IngressAuth::Bearer,
    // An API key (`sk-ant-api…`) goes as `x-api-key` with leading whitespace trimmed, an OAuth
    // token (`sk-ant-oat…`) as `authorization: Bearer`, and any other credential by the lane's mode
    // (configured key as `x-api-key`, forwarded caller token as a bearer). One scheme per request,
    // never both: no native client sends two.
    egress_auth_headers: None,
    egress_auth_lane_constant: false,
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
    stop_sequence_cap: None,
    cache_markers_model_gated: false,
    fills_thought_signature: false,
    frame_after_message_start: Some(ANTHROPIC_PING_SSE_FRAME),
    reshapes_body_at_path_base: true,
    max_cache_control_breakpoints: Some(4),
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: false,
    max_citations_per_delta: Some(1),
    // Native-SDK UA (Stainless `<Title>/Python <ver>`). Re-verify against the latest published SDK
    // before a release (`test_egress_ua_versions_are_pinned_and_present` forces a conscious bump).
    // Written on a translated route, on a same-dialect route whose caller sent no user-agent, and on
    // the health probe; a same-dialect caller's own user-agent replaces it.
    egress_user_agent: "Anthropic/Python 0.39.0",
    has_model_in_url: false,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::UNAUTHORIZED.as_u16(),
        busbar_contract::protocol::ERR_TYPE_AUTHENTICATION,
    ),
    ingress_relays_amzn_headers: false,
    ingress_relayed_response_header_names: &[HDR_REQUEST_ID],
    auth_failure_message: "invalid x-api-key",
    uses_array_stream_shim: false,
    has_native_path_not_found: false,
    egress_stream_accept: busbar_contract::protocol::TEXT_EVENT_STREAM,
    models_list_envelope: Some(models_list_envelope),
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: None,
    // Narrower than `claims` on purpose (no `x-api-key`/path): the SDK always sends the version.
    list_models_fingerprint_headers: &[HDR_ANTHROPIC_VERSION],
    // Every request carries the API version, whatever its credential.
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
/// mixed-case alphanumeric token, not lowercase hex). Distinct from the shared lowercase-first
/// `crate::codec::dialect::BASE62_ALPHABET`: `synth_id_with_prefix` (body ids) needs this one,
/// `synth_anthropic_request_id` (response-header id) deliberately uses the shared one.
const ANTHROPIC_NATIVE_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The response header a native Anthropic endpoint always carries (the SDK reads it into
/// `request_id`). Surfaced externally only via the writer vtable.
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
/// position in `req.messages` (system-role messages excluded). Each entry is `{"m": <message
/// index>, "i": <block index>, "block": <raw block JSON>}`. `read_block`'s empty-Text placeholder
/// keeps the block's shape in the turn; this sentinel lets an Anthropic-to-Anthropic hop through
/// the IR splice the original block back. It is cleared on a cross-protocol hop.
const ANTHROPIC_UNMODELED_BLOCKS_SENTINEL: &str = "__busbar_anthropic_unmodeled_blocks";

/// Every content-block kind [`read_block`] reads (the request and answer grammar, `codec::drops`).
/// A block of any other kind does not cross a translate attempt, which names it.
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
    // `disable_parallel_tool_use`, inside whichever `tool_choice` arm the caller sent.
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
    // The raw unmodelled blocks, for a same-dialect write; `REQUEST_BLOCKS` names them.
    crate::codec::drops::Parked {
        key: ANTHROPIC_UNMODELED_BLOCKS_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    // The thinking ask and the legacy structured-output spelling cross as the IR's reasoning and
    // response format.
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
/// Neither has a neutral form, so only this writer re-emits it; any other writer drops it with a
/// warn.
const VENDOR_NAME: &str = "anthropic";

/// Native `document` content block type (a PDF/text attachment).
const BLOCK_TYPE_DOCUMENT: &str = "document";

/// The one mime an Anthropic `text` document source carries.
const DOCUMENT_MIME_TEXT_PLAIN: &str = "text/plain";

/// The one mime an Anthropic `base64` document source accepts.
const DOCUMENT_MIME_PDF: &str = keys::APPLICATION_PDF;

/// `document.source` for inline bytes, by mime: a PDF rides the `base64` source; `text/*` rides the
/// `text` source as decoded text (`media_type: "text/plain"`) since the IR carries it base64.
/// `None` for any other mime or non-UTF-8 text; the caller drops the block with a warn rather than
/// ship a certain 400 or a corrupt document.
fn inline_document_source(media_type: &str, data: &str) -> Option<serde_json::Value> {
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
/// not hold whole, pushing `{"m","i","block"}` entries (`i` is the raw index). A block
/// [`read_blocks`] leaves out of the IR is marked `"insert": true`; every other entry replaces the
/// IR block at that raw position.
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
        // A `document` is modelled (`IrBlock::Media`) but Media has no slot for `document.context`
        // or `document.citations`. A document carrying either is also parked verbatim so
        // `write_message` splices it back byte-exact on an Anthropic-to-Anthropic hop; a
        // cross-protocol egress falls back to the Media projection and drops them. A document
        // without them is not parked (Media round-trips it losslessly).
        let document_needs_stash = block_type == BLOCK_TYPE_DOCUMENT
            && (block_val.get(keys::CONTEXT).is_some() || block_val.get(keys::CITATIONS).is_some());
        if !is_read_into_ir(block_val) {
            // Not in the IR at all: a same-dialect write INSERTS it back at raw index `i`.
            sink.push(serde_json::json!({
                (keys::M): m, (keys::I): i, (keys::BLOCK): block_val, (STASH_INSERT): true
            }));
        } else if !is_modeled_anthropic_block_type(block_type) || document_needs_stash {
            // In the IR, but not whole: a same-dialect write REPLACES the IR block at raw index
            // `i` with the original.
            sink.push(serde_json::json!({ (keys::M): m, (keys::I): i, (keys::BLOCK): block_val }));
        }
    }
}

/// The stashed raw block for position `(m, i)`, for [`write_message`] to splice over the empty-Text
/// placeholder.
fn find_stashed_block(
    sentinel: &[serde_json::Value],
    m: usize,
    i: usize,
) -> Option<serde_json::Value> {
    sentinel.iter().find_map(|entry| {
        let em = entry.get(keys::M)?.as_u64()? as usize;
        let ei = entry.get(keys::I)?.as_u64()? as usize;
        (em == m && ei == i && !is_insert(entry))
            .then(|| entry.get(keys::BLOCK).cloned())
            .flatten()
    })
}

/// Whether a stash entry is a block the IR does not hold at all.
fn is_insert(entry: &serde_json::Value) -> bool {
    entry.get(STASH_INSERT).and_then(|v| v.as_bool()) == Some(true)
}

/// The blocks of turn `m` the IR does not hold at all, by raw index, in order.
fn stashed_inserts(sentinel: &[serde_json::Value], m: usize) -> Vec<(usize, serde_json::Value)> {
    let mut out: Vec<(usize, serde_json::Value)> = sentinel
        .iter()
        .filter(|entry| is_insert(entry))
        .filter_map(|entry| {
            let em = entry.get(keys::M)?.as_u64()? as usize;
            let ei = entry.get(keys::I)?.as_u64()? as usize;
            (em == m)
                .then(|| Some((ei, entry.get(keys::BLOCK)?.clone())))
                .flatten()
        })
        .collect();
    out.sort_by_key(|(i, _)| *i);
    out
}

/// Error `type` strings for error envelopes and in-stream error events. Shared values alias
/// their canonical home; only `timeout_error` is Anthropic-specific.
const ERR_TYPE_OVERLOADED: &str = busbar_contract::protocol::ERR_TYPE_OVERLOADED;
const ERR_TYPE_INVALID_REQUEST: &str = busbar_contract::protocol::ERR_TYPE_INVALID_REQUEST;
const ERR_TYPE_AUTHENTICATION: &str = busbar_contract::protocol::ERR_TYPE_AUTHENTICATION;
const ERR_TYPE_RATE_LIMIT: &str = busbar_contract::protocol::ERR_TYPE_RATE_LIMIT;
const ERR_TYPE_API_ERROR: &str = busbar_contract::protocol::ERR_TYPE_API_ERROR;
const ERR_TYPE_TIMEOUT: &str = "timeout_error";
/// The billing `ErrorResponse.error` type. No cross-dialect alias exists (OpenAI names the same
/// condition `insufficient_quota`).
const ERR_TYPE_BILLING: &str = "billing_error";
const ERR_TYPE_NOT_FOUND: &str = busbar_contract::protocol::ERR_TYPE_NOT_FOUND;
const ERR_TYPE_PERMISSION: &str = busbar_contract::protocol::ERR_TYPE_PERMISSION;
const ERR_TYPE_REQUEST_TOO_LARGE: &str = busbar_contract::protocol::ERR_TYPE_REQUEST_TOO_LARGE;

// One spelling per wire word this dialect speaks more than once; words shared with other
// dialects live in `crate::codec::keys`.
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
const CRED_PREFIX_API_KEY: &str = "sk-ant-api";
const CRED_PREFIX_OAUTH: &str = "sk-ant-oat";

/// The upstream path this writer targets on the Anthropic Messages API.
const PATH_UPSTREAM: &str = "/v1/messages";

/// HTTP status codes that Anthropic (and cross-protocol upstreams) use to signal overload.
const STATUS_OVERLOADED: u16 = 503;
const STATUS_ANTHROPIC_OVERLOADED: u16 = 529;

/// Upper bound on an upstream-supplied streaming block index. The index flows into the IR and then
/// into writers that allocate against it, so a hostile backend sending up to `u64::MAX` could drive
/// a pathological allocation; every read site clamps to this, as the Bedrock, OpenAI and Cohere
/// readers do with their own caps.
const MAX_ANTHROPIC_BLOCK_INDEX: u64 = 1023;

/// A streaming event's `index`, clamped to `MAX_ANTHROPIC_BLOCK_INDEX`. `None` (the event is
/// dropped) when it is missing or non-numeric; unlike the Bedrock reader, which defaults to 0.
fn read_clamped_block_index(data: &serde_json::Value) -> Option<usize> {
    data.get(keys::INDEX)
        .and_then(|i| i.as_u64())
        .map(|v| v.min(MAX_ANTHROPIC_BLOCK_INDEX) as usize)
}

#[derive(Clone)]
pub struct AnthropicReader;

/// Map a streaming `error.type` token to its breaker `StatusClass`, so a mid-stream error drives
/// the same breaker transition an equivalent HTTP error would (`overloaded_error`, `rate_limit_error`,
/// `api_error` and `timeout_error` are transient; authentication, permission and billing are hard
/// down; the caller-fault types do not penalize the lane). An absent or unrecognized type falls back
/// to `ClientError`, which records nothing, so an unknown error never trips a healthy lane.
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
/// mints. The record is dropped, observably; its citations still ride the text blocks.
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
/// `thinking` gets `signature: ""` (matching the `content_block_start` seed). A value the source
/// reported is left untouched.
fn write_response_block(block: &crate::codec::ir::IrBlock) -> serde_json::Value {
    let mut val = write_block(block);
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
/// so none is silently dropped. Absent/`null` yields `None`; an unrecognized `type` is a client
/// error.
fn read_cache_control(
    val: Option<&serde_json::Value>,
) -> Result<Option<crate::codec::ir::CacheControl>, IrError> {
    let Some(cc_val) = val else { return Ok(None) };
    let Some(cc_obj) = cc_val.as_object() else {
        return Ok(None);
    };
    match cc_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
        Some(CACHE_KIND_EPHEMERAL) => Ok(Some(crate::codec::ir::CacheControl {
            kind: crate::codec::ir::CacheKind::Ephemeral,
        })),
        None => Ok(None),
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
/// `any` is `Required`, `tool` plus `name` is the targeted `Tool{name}`. An absent field or an
/// unrecognized or nameless `tool` shape is `None` (omitted), so no spurious directive appears.
fn read_anthropic_tool_choice(
    val: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    let obj = val?.as_object()?;
    match obj.get(keys::TYPE).and_then(|t| t.as_str())? {
        keys::AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
        keys::NONE_WORD => Some(crate::codec::ir::IrToolChoice::None),
        keys::ANY => Some(crate::codec::ir::IrToolChoice::Required),
        keys::TOOL => obj.get(keys::NAME).and_then(|n| n.as_str()).map(|name| {
            crate::codec::ir::IrToolChoice::Tool {
                name: name.to_string(),
            }
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
/// a foreign content-filter stop is the provider declining to answer, which is what `refusal`
/// tells a client (`end_turn` would call a filtered answer complete). `error`/`other` degrade to
/// `end_turn` rather than leak an off-spec value a strict SDK rejects.
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
/// `refusal` stop's `stop_details` becomes [`crate::codec::ir::IrStopDetail::Refusal`]. Shared by
/// the buffered and stream readers.
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
    // Search-result slot: a RAG passage read from a Converse `searchResult` (or an Anthropic
    // `search_result` whose parked raw block `write_message` did not splice) re-emits natively.
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
                let arr: Vec<serde_json::Value> = citations.iter().map(write_citation).collect();
                obj.insert(keys::CITATIONS.to_string(), serde_json::Value::Array(arr));
            }
            serde_json::Value::Object(obj)
        }
        // A redacted reasoning block (opaque encrypted bytes in `text`) re-emits as
        // `redacted_thinking`, so the bytes never leak as visible `thinking` text.
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
            // Anthropic has no wire concept of a Gemini thoughtSignature, so it is unused.
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
                // A tool_result's content is filtered as a message's is: an attachment with no
                // Anthropic projection is omitted, never sent as the empty-text placeholder
                // Anthropic rejects. A structured JSON result (Bedrock `{"json":…}`) rides as a
                // `text` block of the serialized JSON. An empty text block is omitted too; a
                // result left with nothing is the empty-string content Anthropic accepts.
                let kept: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|b| attachment_is_sendable(b))
                    .filter(
                        |b| !matches!(b, crate::codec::ir::IrBlock::Text { text, .. } if text.is_empty()),
                    )
                    .map(|b| match b {
                        crate::codec::ir::IrBlock::Json(v) => serde_json::json!({
                            (keys::TYPE): keys::TEXT,
                            (keys::TEXT): serde_json::to_string(v).unwrap_or_default(),
                        }),
                        other => write_block(other),
                    })
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
            // S3/FileId references have no Anthropic projection and are filtered before
            // write_block; the arm here is a defensive empty placeholder.
            let mut img = match source {
                crate::codec::ir::IrImageSource::Url(url) => {
                    serde_json::json!({ (keys::TYPE): keys::IMAGE, (keys::SOURCE): { (keys::TYPE): keys::URL, (keys::URL): url } })
                }
                crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                    // Validate the media type: Anthropic accepts only `image/{jpeg,png,gif,webp}`
                    // and 400s anything else, and a non-image mime can reach here (the Gemini
                    // reader maps every `inlineData` onto an Image). Emit the empty placeholder
                    // (`write_message` filters first) rather than a block Anthropic rejects.
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
                // This protocol's own Files-API `file_id` source: re-emit verbatim. A foreign
                // vendor handle is filtered before `write_block` (`attachment_is_sendable`).
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
            // Anthropic's only attachment block is `document` (where an OpenAI `file` part or a
            // Bedrock `document` lands); Audio/Video are dropped with a warn. `write_message`
            // filters those first; the placeholder below is defensive.
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
                crate::codec::ir::IrImageSource::Url(url) => {
                    serde_json::json!({ (keys::TYPE): keys::URL, (keys::URL): url })
                }
                crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                    // A PDF is the `base64` source; text is the `text` source carrying decoded text
                    // (base64 there would hand the model gibberish). A mime with no Anthropic
                    // source is filtered before `write_block`; the placeholder is defensive.
                    match inline_document_source(media_type, data) {
                        Some(src) => src,
                        None => {
                            return serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "" })
                        }
                    }
                }
                // This protocol's own opaque source (`file_id` or `content` document): re-emit
                // verbatim. A foreign vendor reference is filtered in `write_message`.
                crate::codec::ir::IrImageSource::Vendor { value, .. } => value.clone(),
            };
            let mut doc =
                serde_json::json!({ (keys::TYPE): BLOCK_TYPE_DOCUMENT, (keys::SOURCE): src });
            if let Some(obj) = doc.as_object_mut() {
                if let Some(n) = name {
                    obj.insert(keys::TITLE.to_string(), serde_json::json!(n));
                }
                // The document's citation toggle and context hint (Anthropic / Bedrock).
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
            // A structured-json tool-result block has no top-level Anthropic shape; the ToolResult
            // arm filters it first. Defensive empty placeholder.
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
/// projects them onto its own top and `anthropic_effort_word` writes them back verbatim. `minimal`
/// is not read.
fn read_anthropic_effort_word(word: &str) -> Option<crate::codec::ir::IrReasoningEffort> {
    crate::codec::ir::IrReasoningEffort::parse_extended(word)
        .filter(|e| *e != crate::codec::ir::IrReasoningEffort::Minimal)
}

/// `output_config.format` (or the deprecated `output_format`) to the typed IR directive. Only the
/// `json_schema` form with an object schema is one; anything else yields `None` (the key still
/// rides `extra` same-protocol). There is no name/description sibling, so those and `strict` are
/// left unset rather than asserting a flag the caller never wrote.
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
/// Walks only schema-bearing keywords, never a data position like `enum`/`const`/`default`.
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
/// Returns `false` (after a warn naming the construct) for a block with no valid Anthropic
/// projection, so the caller omits it instead of emitting `write_block`'s empty-text placeholder,
/// which Anthropic rejects.
///
/// Dropped: an image that is a foreign vendor handle (a Responses `file_id`, a Bedrock
/// `s3Location`) or whose mime is not `image/{jpeg,png,gif,webp}`; audio/video; a document that is
/// a foreign vendor handle or inline bytes of a mime with no source (see
/// [`inline_document_source`]). This protocol's own `file_id` handles are kept.
fn attachment_is_sendable(block: &crate::codec::ir::IrBlock) -> bool {
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
            crate::codec::ir::IrImageSource::Url(_) => true,
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
                    if inline_document_source(media_type, data).is_some() {
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
    m: usize,
    unmodeled_sentinel: &[serde_json::Value],
) -> serde_json::Value {
    let role_str = match msg.role {
        crate::codec::ir::IrRole::User => keys::USER,
        crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
        // There is no `system` role inside `messages`; `write_request` folds system messages into
        // the top-level `system` array, so this arm is unreachable there. Map to `"user"` so a
        // direct `write_message` call never emits a `role:"system"`.
        crate::codec::ir::IrRole::System => keys::USER,
        // Anthropic has no "tool" message role — tool results are carried as `user` messages whose
        // content holds `tool_result` block(s). (Reachable when translating an OpenAI `tool` message.)
        crate::codec::ir::IrRole::Tool => keys::USER,
    };
    // Request-side filter (write_response/_event call write_block directly, so response reasoning
    // still surfaces). Anthropic 400s an assistant plaintext `thinking` block without a
    // `signature`, and a cross-protocol IR may carry one, so drop those.
    //
    // A redacted thinking block re-emits as `redacted_thinking` with opaque `data` and no
    // signature, which Anthropic accepts. The `redacted: false` guard below is load-bearing:
    // without it every redacted block is dropped, losing the encrypted reasoning a multi-turn
    // extended-thinking conversation replays. Only unsigned plaintext thinking is dropped.
    let mut dropped_unsigned_thinking = 0usize;
    // Enumerate before the drop filter: `find_stashed_block` keys on the raw pre-filter index
    // `read_request` recorded, so collapsing dropped blocks would misalign every later lookup.
    // The IR holds no block for a raw block it did not read; those return at their raw positions,
    // so the walk counts raw positions, not IR indexes.
    let mut inserts = stashed_inserts(unmodeled_sentinel, m)
        .into_iter()
        .peekable();
    let mut raw = 0usize;
    let mut blocks: Vec<serde_json::Value> = Vec::with_capacity(msg.content.len());
    for block in &msg.content {
        while inserts.peek().is_some_and(|(at, _)| *at <= raw) {
            if let Some((_, b)) = inserts.next() {
                blocks.push(b);
            }
            raw += 1;
        }
        let i = raw;
        raw += 1;
        let written: Option<serde_json::Value> = 'block: {
            if let crate::codec::ir::IrBlock::Thinking {
                signature,
                redacted: false,
                signature_origin,
                ..
            } = block
            {
                // A signature minted by another vendor is not valid on Anthropic (400, as for a
                // missing one). Only an Anthropic-origin or unknown-origin signature is sent.
                let foreign = signature_origin
                    .is_some_and(|o| o != crate::codec::ir::IrSignatureOrigin::Anthropic);
                if signature.is_none() || foreign {
                    dropped_unsigned_thinking += 1;
                    break 'block None;
                }
            }
            if !attachment_is_sendable(block) {
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
            // A parked block (e.g. a `document` with `context`) at this raw position: splice the
            // original back over its IR projection.
            if let Some(original) = find_stashed_block(unmodeled_sentinel, m, i) {
                break 'block Some(original);
            }
            if let crate::codec::ir::IrBlock::HostedToolRecord { .. } = block {
                warn_hosted_record_dropped();
                break 'block None;
            }
            Some(write_block(block))
        };
        blocks.extend(written);
    }
    blocks.extend(inserts.map(|(_, b)| b));
    if dropped_unsigned_thinking > 0 {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::THINKING,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [dropped = dropped_unsigned_thinking,],
            "dropped assistant thinking block(s) with no Anthropic signature (none, or another \
             vendor's, IR-18) from anthropic request egress (anthropic rejects them with a 400)"
        );
    }
    // When no blocks survive (e.g. all unsigned thinking was dropped), emit `[]`, not `""`:
    // Anthropic 400s an empty-string `content` but accepts an empty array.
    let content_val: serde_json::Value = serde_json::Value::Array(blocks);
    serde_json::json!({ (keys::ROLE): role_str, (keys::CONTENT): content_val })
}

/// Project one IR tool onto `tools[]`. `None` for a hosted tool that is not Anthropic-defined (a
/// Responses `{"type":"web_search"}` has no `name`); as a function tool it would 400.
fn write_tool(tool: &crate::codec::ir::IrTool) -> Option<serde_json::Value> {
    if let Some(hosted) = &tool.hosted {
        // An Anthropic-defined tool (non-`custom` `type`) always carries its `name`; re-emit it
        // verbatim. Anything else is a foreign hosted tool.
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
             no Anthropic projection");
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
    // Per-tool `strict`: the same schema-guaranteed-arguments contract as OpenAI's `function.strict`.
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
    open_block_indices: std::collections::BTreeSet<usize>,
    /// This stream's `message.id`, minted once and replayed on every later `message_start`. The
    /// Anthropic reader emits `MessageStart` 1:1 with the upstream frame, so a duplicate can reach
    /// this writer; a native id is fixed for the life of the message, so the first one wins.
    message_id: Option<String>,
}

/// Value-namespace constructor for [`AnthropicWriter`]. `protocol_for` builds a fresh writer per
/// stream, so each use inlines an independent empty set. `clippy::declare_interior_mutable_const`
/// is suppressed deliberately: a shared `static` would leak one stream's open indices into another.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const AnthropicWriter: AnthropicWriter = AnthropicWriter {
    stream: StreamCell::new(AnthropicStream {
        open_block_indices: std::collections::BTreeSet::new(),
        message_id: None,
    }),
};

/// A fresh writer as a value, for one-shot `write_request` / `write_response` calls in tests.
/// Borrowing the const directly is `clippy::borrow_interior_mutable_const`; a test driving a
/// sequence of stream events must bind one writer to a local instead of calling this per event.
#[cfg(test)]
pub(crate) fn anthropic_writer() -> AnthropicWriter {
    AnthropicWriter
}

impl AnthropicWriter {
    /// THE STREAM'S `message.id`: the first one wins.
    ///
    /// Returns the id already captured for this stream if there is one, otherwise captures and
    /// returns `mint()`. A duplicate `message_start` therefore re-states the identity the client
    /// already has instead of announcing a second one. Lock poisoning degrades to the freshly
    /// minted id rather than panicking on the request path.
    fn carried_message_id(&self, mint: impl FnOnce() -> String) -> String {
        self.stream
            .lock()
            .message_id
            .get_or_insert_with(mint)
            .clone()
    }

    /// Record that a `content_block_start` was WRITTEN for IR block `index`, so it owes a closing
    /// `content_block_stop`. Returns true when the index was newly opened, false when it was
    /// already open — a redacted-thinking block's start is deferred to its delta, and the boolean
    /// is what keeps a second delta from writing a duplicate start for a block already open on the
    /// wire. Lock poisoning degrades to `false` rather than panicking.
    fn mark_block_open(&self, index: usize) -> bool {
        self.stream.lock().open_block_indices.insert(index)
    }

    /// Consume the open record for `index`, returning whether this writer opened it (and so owes
    /// the closing frame). An untracked index is a block whose start had no Anthropic projection.
    /// Lock poisoning degrades to `false` — closing nothing — rather than panicking.
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
