// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! OpenAI protocol reader/writer implementation.

use crate::codec::dialect::ir_parse_error;
use crate::codec::ir::{IrStreamEvent, IrUsage};
use crate::codec::keys;
use busbar_contract::http::StatusCode;

use crate::codec::dialect::{bearer_error_code, context_length_prose_scan};

use crate::codec::framework::{ERR_TYPE_OVERLOADED, ERR_TYPE_SERVER_ERROR};
use crate::codec::protocol::*;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;

#[allow(unused_imports)]
use super::proto_codec::*;
use crate::codec::egress_prep::{LaneCaps, MaxOutputKey};
use crate::codec::usage_count::{CountRead, CountSlot, UsageCount};

#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
pub mod handler;
mod reader;
pub(crate) mod slots;
mod writer;

/// Build this dialect's wire codec (a fresh instance per resolution).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, OpenAiReader, OpenAiWriter)
}

/// OpenAI's `GET /v1/models` envelope: `model` objects (`owned_by: "busbar"`, `created: 0`) in
/// `{ "object": "list", "data": [...] }`.
fn models_list_envelope(names: &[&str]) -> serde_json::Value {
    let data: Vec<serde_json::Value> = names
        .iter()
        .map(|id| serde_json::json!({ (keys::ID): id, (keys::OBJECT): keys::MODEL, (CREATED): 0, "owned_by": "busbar" }))
        .collect();
    serde_json::json!({ (keys::OBJECT): LIST, (keys::DATA): data })
}

/// Router detection: `/v1/chat/completions` (7), then the OpenAI-family JSON/audio/image ops
/// (`/v1/embeddings`, `/v1/moderations`, `/v1/images/…`, `/v1/audio/…`; 14, the loosest path
/// claims).
fn claims(
    _h: crate::codec::protocol::HeadFields<'_>,
    path: &str,
) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::protocol::ClaimStrength;
    if path.ends_with("/v1/chat/completions") {
        return Some(ClaimStrength(7));
    }
    if path.ends_with("/v1/embeddings")
        || path.ends_with("/v1/moderations")
        || path.contains("/v1/images/")
        || path.contains("/v1/audio/")
    {
        return Some(ClaimStrength(14));
    }
    None
}

/// Headerless detection. A `/v1/models/{id}` no sibling claimed more tightly (25, looser than
/// Gemini's `/v1/models/{id}:{action}` at 20, so a genuine Gemini action wins and a colon-bearing
/// OpenAI fine-tune id stays OpenAI) and an exact `/v1/chat/completions` (55).
fn residual_claims(path: &str) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::protocol::ClaimStrength;
    if path.starts_with("/v1/models/") {
        return Some(ClaimStrength(25));
    }
    if path == "/v1/chat/completions" {
        return Some(ClaimStrength(55));
    }
    None
}

/// OpenAI's protocol declaration.
pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::OpenAiRequestHandler),
    verbs: &[
        busbar_contract::operation::OpVerb::CHAT,
        busbar_contract::operation::OpVerb::EMBEDDINGS,
        busbar_contract::operation::OpVerb::MODERATION,
        busbar_contract::operation::OpVerb::IMAGE,
        busbar_contract::operation::OpVerb::TRANSCRIPTION,
        busbar_contract::operation::OpVerb::SPEECH,
    ],
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
    stream_usage_requires_opt_in: true,
    requires_max_tokens: false,
    #[cfg(test)]
    stop_sequence_cap: Some((4, "OpenAI")),
    cache_markers_model_gated: false,
    fills_thought_signature: false,
    frame_after_message_start: None,
    max_cache_control_breakpoints: None,
    #[cfg(test)]
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: true,
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
    models_list_envelope: Some(models_list_envelope),
    claims: Some(claims),
    residual_claims: Some(residual_claims),

    residual_default: true,
    vendor_response_metadata: None,

    list_models_fingerprint_headers: &[],
    static_headers: &[],
};

/// This dialect's registration (listed in `crate::codec::DIALECTS`).
pub(crate) const ENTRY: super::proto_codec::DialectEntry = super::proto_codec::DialectEntry {
    decl: &DECL,
    protocol,
    with_writer: |f| {
        let w = OpenAiWriter;
        f(&w)
    },
    with_reader: |f| f(&OpenAiReader),
    leaf: &handler::LEAF,
};

/// Largest upstream `tool_calls[].index` accepted in a streaming chunk (OpenAI documents at most 128
/// parallel calls, so 127 is the highest valid 0-based index).
const MAX_TOOL_INDEX: u64 = 127;

/// Cap on distinct tool-call indices tracked per stream (`open_tools`), bounding memory and
/// synthesized BlockStart events against a backend emitting unbounded unique indices.
const MAX_OPEN_TOOLS: usize = crate::codec::dialect::MAX_OPEN_TOOL_CALLS;

/// OpenAI Chat's usage counts, as data. `prompt_tokens` already includes the cached prefix
/// (`prompt_tokens_details.cached_tokens`), which is subtracted (saturating) and carried as the
/// IR's additive cache read.
const USAGE: &[UsageCount] = &[
    (CountSlot::Input, CountRead::Zero(&[PROMPT_TOKENS])),
    (
        CountSlot::Input,
        CountRead::Less(&[PROMPT_TOKENS_DETAILS, keys::CACHED_TOKENS]),
    ),
    (CountSlot::Output, CountRead::Zero(&[COMPLETION_TOKENS])),
    (
        CountSlot::CacheRead,
        CountRead::Opt(&[PROMPT_TOKENS_DETAILS, keys::CACHED_TOKENS]),
    ),
    (
        CountSlot::Reasoning,
        CountRead::Lenient(&[COMPLETION_TOKENS_DETAILS, keys::REASONING_TOKENS]),
    ),
    (
        CountSlot::InputAudio,
        CountRead::Lenient(&[PROMPT_TOKENS_DETAILS, AUDIO_TOKENS]),
    ),
    (
        CountSlot::OutputAudio,
        CountRead::Lenient(&[COMPLETION_TOKENS_DETAILS, AUDIO_TOKENS]),
    ),
    (
        CountSlot::AcceptedPrediction,
        CountRead::Lenient(&[COMPLETION_TOKENS_DETAILS, ACCEPTED_PREDICTION_TOKENS]),
    ),
    (
        CountSlot::RejectedPrediction,
        CountRead::Lenient(&[COMPLETION_TOKENS_DETAILS, REJECTED_PREDICTION_TOKENS]),
    ),
];

/// Stable identifier of the identity [`read_openai_usage`] checks `usage.total_tokens` against,
/// carried on [`crate::codec::ir::UsageIdentityNote::identity`].
const OPENAI_USAGE_IDENTITY: &str = "openai.usage";

/// An OpenAI Chat `usage` object (`None` when absent) to the IR usage through [`USAGE`]; the
/// serving tier (`service_tier`, a top-level member beside `usage`) is a word, set from `tier`.
fn read_openai_usage(
    usage: Option<&serde_json::Value>,
    tier: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let mut ir = crate::codec::usage_count::read_usage(VENDOR_NAME, usage, USAGE)?;

    ir.detail.usage_identity_note = (!crate::codec::usage_count::refused_any(usage, USAGE))
        .then(|| {
            crate::codec::usage_count::stated_total_note(
                VENDOR_NAME,
                OPENAI_USAGE_IDENTITY,
                usage.and_then(|u| u.get(keys::TOTAL_TOKENS)),
                &mut ir,
            )
        })
        .flatten();
    ir.detail.service_tier = crate::codec::carry::read_word(map::WORDS_SERVED_TIER, tier);
    ir.detail.by_modality = usage.and_then(read_by_modality);
    Ok(ir)
}

/// `{prompt,completion}_tokens_details.{text,image,audio}_tokens` to the IR's by-modality split
/// (presentation only, never billed).
fn read_by_modality(usage: &serde_json::Value) -> Option<crate::codec::ir::IrUsageByModality> {
    let side = |details: &str| {
        let d = usage.get(details);
        let n = |k: &str| d.and_then(|d| d.get(k)).and_then(|v| v.as_u64());
        crate::codec::ir::IrModalityCounts {
            text: n(TEXT_TOKENS),
            image: n(IMAGE_TOKENS),
            audio: n(AUDIO_TOKENS),
            video: None,
        }
    };
    let input = side(PROMPT_TOKENS_DETAILS);
    let output = side(COMPLETION_TOKENS_DETAILS);
    (input.text.is_some() || input.image.is_some() || output.text.is_some()).then(|| {
        crate::codec::ir::IrUsageByModality {
            input,
            output,
            ..Default::default()
        }
    })
}

/// The text slice of a `*_tokens_details` object.
const TEXT_TOKENS: &str = "text_tokens";
/// The image slice of `prompt_tokens_details`.
const IMAGE_TOKENS: &str = "image_tokens";

/// Fallback `model` stamped onto a cross-protocol response when the backend supplied none (e.g.
const DEFAULT_MODEL: &str = crate::codec::dialect::FALLBACK_MODEL;

/// Sentinel key tracking the `max_completion_tokens` spelling. The reader folds `max_tokens` and
/// `max_completion_tokens` into the one IR `max_tokens`, but o1/o3 reasoning models reject
/// `max_tokens`, so the source spelling is recorded in `extra` for the writer to re-emit on a
/// same-protocol passthrough.
const MAX_COMPLETION_TOKENS_SENTINEL: &str = "__busbar_max_completion_tokens";

/// Sentinel key parking per-message provider-specific fields no typed IR models (an assistant
/// turn's `audio` reference, legacy `function_call`, future keys), as an object keyed by IR-message
/// index.
const MESSAGE_EXTRAS_SENTINEL: &str = "__busbar_openai_message_extras";

/// Key inside one message's `MESSAGE_EXTRAS_SENTINEL` entry marking a legacy `role:"function"`
const LEGACY_FUNCTION_ROLE_KEY: &str = "__busbar_legacy_function_role";

use crate::codec::dialect::MESSAGE_NAMES_SENTINEL;

const OBJ_COMPLETION: &str = "chat.completion";
const OBJ_CHUNK: &str = "chat.completion.chunk";

const FINISH_STOP: &str = "stop";
const FINISH_LENGTH: &str = "length";
const FINISH_TOOL_CALLS: &str = keys::TOOL_CALLS;
const FINISH_CONTENT_FILTER: &str = "content_filter";
/// Legacy `finish_reason` for function-calling (pre-tool_calls era).
const FINISH_FUNCTION_CALL: &str = keys::FUNCTION_CALL;

const RESP_FORMAT_TEXT: &str = keys::TEXT;
const RESP_FORMAT_JSON_SCHEMA: &str = keys::JSON_SCHEMA;
const RESP_FORMAT_JSON_OBJECT: &str = "json_object";

const TOOL_TYPE_FUNCTION: &str = keys::FUNCTION;
/// Tool `type` for a custom (free-text / grammar input) tool.
const TOOL_TYPE_CUSTOM: &str = keys::CUSTOM;

/// Prefix of every native chat-completion id (`chatcmpl-<24 base62 chars>`).
const COMPLETION_ID_PREFIX: &str = "chatcmpl-";

const PATH_UPSTREAM: &str = "/v1/chat/completions";

/// The bad-key 401 message, the official phrasing so SDK `is_auth_error` helpers that key on the
/// message string still fire.
const AUTH_FAILURE_MSG: &str = "Incorrect API key provided.";

/// The dialect name, also the `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`] this
/// protocol produces (an OpenAI `file.file_id`, which has no neutral form).
const VENDOR_NAME: &str = "openai";

const LANGUAGE: &str = "language";
const W_DURATION: &str = "duration";
const SEGMENTS: &str = "segments";
const AVG_LOGPROB: &str = "avg_logprob";
const NO_SPEECH_PROB: &str = "no_speech_prob";
const COMPRESSION_RATIO: &str = "compression_ratio";
const W_WORDS: &str = "words";
const WORD: &str = "word";
const SECONDS: &str = "seconds";
const SPEED: &str = "speed";
const ENCODING_FORMAT: &str = "encoding_format";
const LIST: &str = "list";
const PROMPT_TOKENS: &str = "prompt_tokens";
const W_N: &str = "n";
const SIZE: &str = "size";
const STYLE: &str = "style";
const BACKGROUND: &str = "background";
const OUTPUT_COMPRESSION: &str = "output_compression";
const B64_JSON: &str = "b64_json";
const REVISED_PROMPT: &str = "revised_prompt";
const CREATED: &str = "created";
const FLAGGED: &str = "flagged";
const CATEGORIES: &str = "categories";
const CATEGORY_SCORES: &str = "category_scores";
const CATEGORY_APPLIED_INPUT_TYPES: &str = "category_applied_input_types";
const FILE: &str = "file";
const AUDIO_WAV: &str = "audio/wav";
const PROMPT_TOKENS_DETAILS: &str = "prompt_tokens_details";
const COMPLETION_TOKENS: &str = "completion_tokens";
const COMPLETION_TOKENS_DETAILS: &str = "completion_tokens_details";
const AUDIO_TOKENS: &str = "audio_tokens";
const ACCEPTED_PREDICTION_TOKENS: &str = "accepted_prediction_tokens";
const REJECTED_PREDICTION_TOKENS: &str = "rejected_prediction_tokens";
const AUDIO: &str = "audio";
const FORMAT_MP3: &str = "mp3";
const CHOICES: &str = "choices";
const MAX_COMPLETION_TOKENS: &str = "max_completion_tokens";
const FUNCTIONS: &str = "functions";
const REASONING_EFFORT: &str = "reasoning_effort";
const REASONING_CONTENT: &str = "reasoning_content";
const SYSTEM_FINGERPRINT: &str = "system_fingerprint";

/// The `model` to emit: the upstream value, else [`DEFAULT_MODEL`], so the required `model` field
/// is never omitted.
fn model_or_default(model: Option<&str>) -> &str {
    model.unwrap_or(DEFAULT_MODEL)
}

/// OpenAI `finish_reason` to [`crate::codec::ir::IrStopReason`].
fn read_openai_stop_reason(token: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match token {
        FINISH_STOP => S::EndTurn,
        FINISH_LENGTH => S::MaxTokens,
        FINISH_TOOL_CALLS | FINISH_FUNCTION_CALL => S::ToolUse,
        FINISH_CONTENT_FILTER => S::Safety,
        _ => S::Other,
    }
}

/// [`crate::codec::ir::IrStopReason`] to `finish_reason` ({stop,length,tool_calls,content_filter});
/// a reason with no analog (`refusal`, `error`, `pause_turn`, `other`) degrades to `stop` rather
/// than leak an off-enum value a strict SDK rejects.
fn write_openai_stop_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::EndTurn | S::StopSequence => FINISH_STOP,
        S::MaxTokens => FINISH_LENGTH,
        S::ToolUse => FINISH_TOOL_CALLS,
        S::Safety => FINISH_CONTENT_FILTER,
        S::Refusal | S::Error | S::PauseTurn | S::Other => FINISH_STOP,
    }
}

/// Read a `response_format` object (`{type:"json_schema", json_schema:{name,schema,strict,
/// description}}`, `{type:"json_object"}` or `{type:"text"}`) into
/// [`crate::codec::ir::IrResponseFormat`].
fn read_openai_response_format(
    v: &serde_json::Value,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let o = v.as_object()?;
    match o.get(keys::TYPE).and_then(|t| t.as_str()) {
        Some(RESP_FORMAT_TEXT) => Some(crate::codec::ir::IrResponseFormat::bare(false, None)),
        Some(RESP_FORMAT_JSON_SCHEMA) => Some(
            crate::codec::ir::IrResponseFormat::from_json_schema_members(o.get(keys::JSON_SCHEMA)),
        ),

        Some(_) => Some(crate::codec::ir::IrResponseFormat::bare(true, None)),
        None => None,
    }
}

/// Project [`crate::codec::ir::IrResponseFormat`] into the native `response_format`.
fn write_openai_response_format(rf: &crate::codec::ir::IrResponseFormat) -> serde_json::Value {
    if !rf.json {
        return serde_json::json!({(keys::TYPE): RESP_FORMAT_TEXT});
    }
    match &rf.schema {
        Some(schema) => {
            let mut js = serde_json::Map::new();
            rf.write_json_schema_members(schema, &mut js);
            serde_json::json!({(keys::TYPE): RESP_FORMAT_JSON_SCHEMA, (keys::JSON_SCHEMA): js})
        }
        None => serde_json::json!({(keys::TYPE): RESP_FORMAT_JSON_OBJECT}),
    }
}

/// Width of a native completion id's suffix: `chatcmpl-` plus exactly 24 base62 characters (33
/// in all).
const COMPLETION_ID_TOKEN_LEN: usize = 24;

/// The shared base62 alphabet (`crate::codec::dialect::BASE62_ALPHABET`), used by
/// [`synth_completion_id`].
const BASE62: &[u8; 62] = crate::codec::dialect::BASE62_ALPHABET;

/// Synthesize a native-shaped completion id (`chatcmpl-<24 base62 chars>`) for cross-protocol
/// responses where the backend supplied none.
fn synth_completion_id(host: &dyn crate::codec::cells::CodecHost) -> Result<String, IrError> {
    let mut token = [b'0'; COMPLETION_ID_TOKEN_LEN];
    if !crate::codec::dialect::fill_base62(host, &mut token, BASE62) {
        return Err(entropy_error());
    }
    let token = std::str::from_utf8(&token).map_err(|_| entropy_error())?;
    Ok(format!("{COMPLETION_ID_PREFIX}{token}"))
}

/// The `error.code` token OpenAI-compatible upstreams use for a rate limit. The specific token
/// rides on `code`; `type` carries the coarser `rate_limit_error` bucket.
const STREAM_ERR_CODE_RATE_LIMIT: &str = "rate_limit_exceeded";

/// Breaker class for an inline mid-stream `{"error":{...}}` chunk, from `error.code` / `error.type`.
fn stream_inline_error_class(error_type: Option<&str>, code: Option<&str>) -> StatusClass {
    let signal = code.filter(|c| !c.is_empty()).or(error_type);
    match signal {
        Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH) => StatusClass::ContextLength,
        Some(STREAM_ERR_CODE_RATE_LIMIT)
        | Some(ERR_TYPE_RATE_LIMIT)
        | Some(ERR_TYPE_INSUFFICIENT_QUOTA) => StatusClass::RateLimit,
        Some(ERR_TYPE_AUTHENTICATION) | Some(ERR_TYPE_PERMISSION) => StatusClass::Auth,
        Some(ERR_TYPE_OVERLOADED) => StatusClass::Overloaded,
        Some(ERR_TYPE_INVALID_REQUEST) | Some(ERR_TYPE_NOT_FOUND) => StatusClass::ClientError,
        Some(ERR_TYPE_SERVER_ERROR) => StatusClass::ServerError,
        Some(_unrecognized) => StatusClass::ServerError,
        None => StatusClass::ServerError,
    }
}

/// OpenAI reader implementation.
#[derive(Clone)]
pub struct OpenAiReader;

/// Project an [`crate::codec::ir::IrBlock::Media`] into the Chat content part that can express it,
/// or `None` when there is no slot (the caller emits nothing).
fn media_part_from_ir(
    kind: crate::codec::ir::IrMediaKind,
    source: &crate::codec::ir::IrImageSource,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    use crate::codec::ir::{IrImageSource as S, IrMediaKind as K};
    match (kind, source) {
        (K::Audio, S::Base64 { media_type, data }) => match openai_audio_input_format(media_type) {
            Some(format) => Some(serde_json::json!({
                (keys::TYPE): keys::INPUT_AUDIO,
                (keys::INPUT_AUDIO): { (keys::DATA): data, (keys::FORMAT): format }
            })),
            None => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::AUDIO,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [media_kind = AUDIO, mime = media_type.as_str(),],
                    "dropping audio attachment on OpenAI Chat egress: input_audio.format is a \
                         closed {{wav, mp3}} enum and this mime maps to neither; the block is NOT \
                         emitted (deliberately absent, not an invalid format the API would 400 on)"
                );
                None
            }
        },
        (K::Document, S::Base64 { media_type, data }) => {
            let mut file = serde_json::Map::new();
            file.insert(
                keys::FILE_DATA.to_string(),
                serde_json::json!(format!("data:{media_type};base64,{data}")),
            );
            if let Some(n) = name {
                file.insert(keys::FILENAME.to_string(), serde_json::json!(n));
            }
            Some(serde_json::json!({ (keys::TYPE): FILE, (FILE): serde_json::Value::Object(file) }))
        }

        (K::Document, source) if super::url_citation_wire::files_api_id(source).is_some() => {
            let id = super::url_citation_wire::files_api_id(source)?;
            let mut file = serde_json::Map::new();
            file.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
            if let Some(n) = name {
                file.insert(keys::FILENAME.to_string(), serde_json::json!(n));
            }
            Some(serde_json::json!({ (keys::TYPE): FILE, (FILE): serde_json::Value::Object(file) }))
        }
        _ => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::block(kind.as_str()),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [media_kind = kind.as_str(),],
                "dropping attachment on OpenAI Chat egress: this dialect has content parts for \
                 inline audio (`input_audio`) and files (`file`) only — a video block, or an \
                 attachment carried as a bare URL or a foreign vendor handle, has no part to go in. \
                 The block is NOT emitted (it is deliberately absent, not replaced by empty text)"
            );
            None
        }
    }
}

/// Map an audio mime onto `input_audio.format`, or `None` when it has no form in the closed
/// `{wav, mp3}` enum.
fn openai_audio_input_format(media_type: &str) -> Option<&'static str> {
    match media_type.to_ascii_lowercase().as_str() {
        keys::AUDIO_MPEG | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg-3" => Some(FORMAT_MP3),
        AUDIO_WAV | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" | "audio/x-pn-wav" => {
            Some(keys::WAV)
        }
        _ => None,
    }
}

/// The Chat content-part kinds this reader models.
const PART_KINDS: &[&str] = &[
    keys::TEXT,
    keys::IMAGE_URL,
    keys::INPUT_AUDIO,
    FILE,
    keys::REFUSAL,
];

/// The Chat request content-part grammar (`codec::drops`). A part of any other kind does not cross
/// a translate attempt, which names it.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["messages[]", "content[]"],
    tag: Some(keys::TYPE),
    modelled: PART_KINDS,
    companions: &[],
}];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, "type=text"),
    (crate::codec::drops::kind::IMAGE, "type=image_url"),
    (crate::codec::drops::kind::DOCUMENT, "type=file"),
    (crate::codec::drops::kind::AUDIO, "type=input_audio"),
    (crate::codec::drops::kind::TOOL_USE, "tool_calls[]"),
    (crate::codec::drops::kind::TOOL_RESULT, "role=tool"),
];

/// The IR request members the reader carries by code from a path no map-file row names (how a drop
/// of one is named by the caller's wire path).
const REQUEST_CODE_NAMES: &[(&str, &str)] =
    &[(crate::codec::drops::name::TOP_LOGPROBS, keys::TOP_LOGPROBS)];

/// The Chat answer content-part grammar.
const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["choices[]", "message", "content[]"],
    tag: Some(keys::TYPE),
    modelled: PART_KINDS,
    companions: &[],
}];

/// What the Chat reader parks in `extra` beside the members its map file does not model.
const PARKED: &[crate::codec::drops::Parked] = &[
    crate::codec::drops::Parked {
        key: MAX_COMPLETION_TOKENS_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: crate::codec::dialect::MESSAGE_NAMES_SENTINEL,
        holds: crate::codec::drops::Holds::Path(crate::codec::dialect::MESSAGE_NAMES_PATH),
    },
    crate::codec::drops::Parked {
        key: MESSAGE_EXTRAS_SENTINEL,
        holds: crate::codec::drops::Holds::Items("messages[]", &[LEGACY_FUNCTION_ROLE_KEY]),
    },
    crate::codec::drops::Parked {
        key: FUNCTIONS,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::FUNCTION_CALL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::STREAM_OPTIONS,
        holds: crate::codec::drops::Holds::Nothing,
    },
];

/// What this dialect's answers carry beyond its map file's rows. A chunk's identity, model, clock,
/// index, finish reason, logprobs, usage and served tier are read by the stream reader (the stream
/// rows name only the delta).
const RESPONSE_CODE: &[&str] = &[];
const STREAM_CODE: &[&str] = &[
    "id",
    "model",
    CREATED,
    "object",
    "choices[].index",
    "choices[].finish_reason",
    "choices[].logprobs",
    "usage",
    "service_tier",
];

/// Answer paths inside a subtree this dialect carries that its code does not, named by the drop
/// walk (paths a coarse map row covers).
const RESPONSE_DROPS: &[&str] = &["choices[].message.audio.expires_at"];
const STREAM_DROPS: &[&str] = &[
    "usage.completion_tokens_details.text_tokens",
    "usage.prompt_tokens_details.image_tokens",
    "usage.prompt_tokens_details.text_tokens",
];

/// Read one content part: `None` for a kind this reader does not model, which is dropped.
fn read_openai_part(
    block_val: &serde_json::Value,
) -> Result<Option<crate::codec::ir::IrBlock>, IrError> {
    if !REQUEST_BLOCKS.iter().all(|g| g.models(block_val)) {
        return Ok(None);
    }
    read_openai_block(block_val).map(Some)
}

/// Read an OpenAI-format block of a kind [`REQUEST_BLOCKS`] models.
fn read_openai_block(block_val: &serde_json::Value) -> Result<crate::codec::ir::IrBlock, IrError> {
    let obj = block_val.as_object().ok_or_else(ir_parse_error)?;

    let block_type = obj.get(keys::TYPE).and_then(|v| v.as_str()).unwrap_or("");

    match block_type {
        keys::TEXT => {
            let text_val = obj.get(keys::TEXT);
            let text = text_val.and_then(|t| t.as_str()).unwrap_or("").to_string();
            Ok(crate::codec::ir::IrBlock::Text {
                text,
                cache_control: None,
                citations: Vec::new(),
                refusal: false,
            })
        }
        keys::IMAGE_URL => {
            let image_obj = obj.get(keys::IMAGE_URL).ok_or_else(ir_parse_error)?;
            let url = image_obj
                .get(keys::URL)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let detail = image_obj.get(keys::DETAIL).and_then(|v| v.as_str());
            let parsed = detail.and_then(crate::codec::ir::IrImageDetail::parse);
            if let (Some(word), None) = (detail, parsed) {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire("messages[].content[].image_url.detail"),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [detail = word,],
                    "dropping an unknown image_url.detail word: the IR carries auto/low/high only"
                );
            }
            Ok(crate::codec::ir::IrBlock::Image {
                source: super::ir_encode::parse_image_url(url),
                cache_control: None,
                detail: parsed,
            })
        }

        keys::INPUT_AUDIO => {
            let audio_obj = obj.get(keys::INPUT_AUDIO).ok_or_else(ir_parse_error)?;
            let data = audio_obj
                .get(keys::DATA)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let format = audio_obj
                .get(keys::FORMAT)
                .and_then(|v| v.as_str())
                .unwrap_or(keys::WAV);
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

        FILE => {
            let file_obj = obj.get(FILE).ok_or_else(ir_parse_error)?;
            let name = file_obj
                .get(keys::FILENAME)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let source = match file_obj
                .get(keys::FILE_DATA)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                Some(data_uri)
                    if data_uri.starts_with("data:")
                        || data_uri.starts_with("https://")
                        || data_uri.starts_with("http://") =>
                {
                    super::ir_encode::parse_image_url(data_uri)
                }

                Some(bare_base64) => crate::codec::ir::IrImageSource::Base64 {
                    media_type: file_media_type_from_name(name.as_deref()).to_string(),
                    data: bare_base64.to_string(),
                },
                None => {
                    let file_id = file_obj
                        .get(keys::FILE_ID)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    crate::codec::ir::IrImageSource::Vendor {
                        vendor: VENDOR_NAME,
                        value: serde_json::json!({ (keys::FILE_ID): file_id }),
                    }
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

        keys::REFUSAL => {
            let text = obj
                .get(keys::REFUSAL)
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Ok(crate::codec::ir::IrBlock::Text {
                text,
                cache_control: None,
                citations: Vec::new(),
                refusal: true,
            })
        }

        _ => Err(ir_parse_error()),
    }
}

/// The media type of a bare-base64 `file.file_data` payload, from its `filename` extension;
/// `application/octet-stream` when there is none or it is unlisted (no type is invented).
fn file_media_type_from_name(name: Option<&str>) -> &'static str {
    let ext = name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    const CHAT_MEDIA_TYPES: &[(&str, &str)] = &[
        ("htm", "text/html"),
        ("json", "application/json"),
        ("xml", "application/xml"),
        ("jpg", keys::IMAGE_JPEG),
        ("jpeg", keys::IMAGE_JPEG),
        (keys::WAV, AUDIO_WAV),
        (FORMAT_MP3, keys::AUDIO_MPEG),
    ];
    crate::codec::dialect::media_type(
        &[
            crate::codec::dialect::DOCUMENT_MEDIA_TYPES,
            crate::codec::dialect::IMAGE_MEDIA_TYPES,
            CHAT_MEDIA_TYPES,
        ],
        &ext,
    )
    .unwrap_or(keys::APPLICATION_OCTET_STREAM)
}

/// Read an OpenAI-format tool from JSON.
fn read_openai_tool(tool_val: &serde_json::Value) -> Result<crate::codec::ir::IrTool, IrError> {
    let obj = tool_val.as_object().ok_or_else(ir_parse_error)?;

    if let Some(kind) = obj.get(keys::TYPE).and_then(|t| t.as_str()) {
        if kind != TOOL_TYPE_FUNCTION {
            return Ok(crate::codec::ir::IrTool {
                name: String::new(),
                description: None,
                input_schema: serde_json::Value::Null,
                cache_control: None,
                hosted: Some(tool_val.clone()),
                strict: None,
            });
        }
    }

    let src = obj
        .get(keys::FUNCTION)
        .and_then(|f| f.as_object())
        .unwrap_or(obj);

    let name = src
        .get(keys::NAME)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = src
        .get(keys::DESCRIPTION)
        .and_then(|v| v.as_str().map(String::from));
    let input_schema = src
        .get(keys::PARAMETERS)
        .or_else(|| src.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));

    Ok(crate::codec::ir::IrTool {
        name,
        description,
        input_schema,
        cache_control: None,
        hosted: None,

        strict: src.get(keys::STRICT).and_then(|v| v.as_bool()),
    })
}

/// Read `tool_choice` (`"auto"` / `"none"` / `"required"`, or `{"type":"function","function":
fn read_openai_tool_choice(
    val: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    match val? {
        serde_json::Value::String(s) => match s.as_str() {
            keys::AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
            keys::NONE_WORD => Some(crate::codec::ir::IrToolChoice::None),
            keys::REQUIRED => Some(crate::codec::ir::IrToolChoice::Required),
            _ => None,
        },
        serde_json::Value::Object(o) => {
            if o.get(keys::TYPE).and_then(|t| t.as_str()) == Some(TOOL_TYPE_FUNCTION) {
                o.get(keys::FUNCTION)
                    .and_then(|f| f.get(keys::NAME))
                    .and_then(|n| n.as_str())
                    .map(|name| crate::codec::ir::IrToolChoice::Tool {
                        name: name.to_string(),
                    })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The stream-start identity replayed onto every `chat.completion.chunk` (see
/// `OpenAiStreamFraming`), captured from the opening chunk so the whole stream shares one identity.
#[derive(Clone)]
struct OpenAiChunkIdentity {
    id: serde_json::Value,
    created: serde_json::Value,
    model: Option<serde_json::Value>,
}

/// OpenAI-ingress per-stream framing, applying two client-facing quirks the shared
/// `StreamTranslate` must not name: (1) the real API repeats `id`/`created`/`model` on every
/// `chat.completion.chunk` while the writer emits them only on the opening chunk, so this latches
/// and replays them; (2) native `include_usage` puts usage on a separate trailing chunk after the
/// finish_reason chunk, while the 1:1 writer folds it onto the finish chunk, so this un-folds it.
/// Built per stream via [`OpenAiWriter::new_stream_framing`]; runs only on the cross-protocol path.
#[derive(Default)]
struct OpenAiStreamFraming {
    /// The identity latched from the first chunk that carries an `id`, replayed onto later chunks.
    chunk_identity: Option<OpenAiChunkIdentity>,
    /// Raw IR block index to 0-based tool-call ordinal. The writer stamps the IR block index onto
    /// `tool_calls[].index`, but a source can open its first tool_use at a non-zero block (an
    /// Anthropic stream with text at block 0). OpenAI requires `tool_calls[].index` to enumerate
    /// from 0, and SDK accumulators key buffers on it, so a first call at index 1 would be dropped.
    /// Each distinct raw index gets the next ordinal on first sight, keeping parallel calls
    /// distinct. Populated lazily.
    tool_call_index: std::collections::BTreeMap<u64, u64>,
    /// Did the original client request carry `stream_options.include_usage == true`? Busbar always
    /// injects `include_usage` upstream to bill, but a native stream surfaces the trailing
    /// usage-only chunk only when the client opted in. When `false`, `on_egress_chunk` strips the
    /// folded usage so the client never gets an unsolicited `{choices:[], usage}` chunk (a
    /// `choices[0]` IndexError). Defaults `false`; the engine sets it via `set_client_include_usage`.
    client_include_usage: bool,
}

impl StreamFraming for OpenAiStreamFraming {
    fn on_egress_chunk(&mut self, chunk: &mut serde_json::Value) -> Option<serde_json::Value> {
        self.apply_chunk_identity(chunk);
        self.remap_tool_call_index(chunk);
        if self.client_include_usage {
            split_openai_trailing_usage(chunk)
        } else {
            strip_folded_usage(chunk);
            None
        }
    }

    fn folds_terminal_usage(&self) -> bool {
        false
    }

    fn set_client_include_usage(&mut self, include: bool) {
        self.client_include_usage = include;
    }

    /// Same-protocol edit. OpenAI to OpenAI re-emits upstream frames byte-for-byte and never calls
    /// `on_egress_chunk`, so the opted-out strip above cannot fire.
    fn same_proto_edit(&self, payload: &[u8]) -> SameProtoEdit {
        if self.client_include_usage {
            return SameProtoEdit::Keep;
        }

        let (mut usage, mut choices): (Option<&[u8]>, Option<&[u8]>) = (None, None);
        let read = busbar_contract::spans::scan(payload, 0, |m| {
            let value = &payload[m.value_start..m.value_end];
            if m.is(keys::USAGE) {
                usage = Some(value);
            } else if m.is(CHOICES) {
                choices = Some(value);
            }
        });
        let (Some(_), Some(usage), Some(choices)) = (read, usage, choices) else {
            return SameProtoEdit::Keep;
        };
        if !choices.starts_with(b"[") {
            return SameProtoEdit::Keep;
        }
        match crate::codec::json_splice::first_element(choices) {
            Some(_) => SameProtoEdit::StripUsage,
            None if usage.starts_with(b"{") => SameProtoEdit::Drop,
            None => SameProtoEdit::Keep,
        }
    }
}

impl OpenAiStreamFraming {
    /// Remap every `choices[].delta.tool_calls[].index` from the writer's raw IR block index to a
    /// 0-based ordinal: the first distinct raw index becomes 0, the next 1, and a repeat (argument
    /// fragments) replays its ordinal.
    fn remap_tool_call_index(&mut self, chunk: &mut serde_json::Value) {
        let Some(obj) = chunk.as_object_mut() else {
            return;
        };
        if obj.get(keys::OBJECT).and_then(|v| v.as_str()) != Some(OBJ_CHUNK) {
            return;
        }
        let Some(choices) = obj.get_mut(CHOICES).and_then(|c| c.as_array_mut()) else {
            return;
        };
        for choice in choices {
            let Some(tool_calls) = choice
                .get_mut(keys::DELTA)
                .and_then(|d| d.get_mut(keys::TOOL_CALLS))
                .and_then(|tc| tc.as_array_mut())
            else {
                continue;
            };
            for tc in tool_calls {
                let Some(raw) = tc.get(keys::INDEX).and_then(|i| i.as_u64()) else {
                    continue;
                };

                if self.tool_call_index.len() >= MAX_OPEN_TOOLS
                    && !self.tool_call_index.contains_key(&raw)
                {
                    continue;
                }
                let next = self.tool_call_index.len() as u64;
                let ordinal = *self.tool_call_index.entry(raw).or_insert(next);
                if let Some(tc_obj) = tc.as_object_mut() {
                    tc_obj.insert(keys::INDEX.to_string(), serde_json::json!(ordinal));
                }
            }
        }
    }

    /// Latch `id`/`created`/`model` from the first chunk that carries an `id`, and inject them
    /// into every later chunk (the writer omits them there).
    fn apply_chunk_identity(&mut self, chunk: &mut serde_json::Value) {
        let Some(obj) = chunk.as_object_mut() else {
            return;
        };

        if obj.get(keys::OBJECT).and_then(|v| v.as_str()) != Some(OBJ_CHUNK) {
            return;
        }
        match &self.chunk_identity {
            None => {
                if obj.contains_key(keys::ID) {
                    self.chunk_identity = Some(OpenAiChunkIdentity {
                        id: obj
                            .get(keys::ID)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        created: obj.get(CREATED).cloned().unwrap_or(serde_json::Value::Null),
                        model: obj.get(keys::MODEL).cloned(),
                    });
                }
            }
            Some(identity) => {
                obj.entry(keys::ID.to_string())
                    .or_insert_with(|| identity.id.clone());
                obj.entry(CREATED.to_string())
                    .or_insert_with(|| identity.created.clone());
                if let Some(model) = &identity.model {
                    obj.entry(keys::MODEL.to_string())
                        .or_insert_with(|| model.clone());
                }
            }
        }
    }
}

/// Split a folded-usage chunk into (chunk without usage, trailing usage-only chunk). Returns
/// `Some(trailing)` when `chunk` is a `chat.completion.chunk` carrying a folded top-level `usage`
/// object: the usage is removed from `chunk` in place and re-homed onto a trailing chunk shaped like
/// a native include_usage trailer (same `id`/`created`/`model`/`object`, empty `choices`, the
/// `usage`).
fn split_openai_trailing_usage(chunk: &mut serde_json::Value) -> Option<serde_json::Value> {
    let obj = chunk.as_object_mut()?;
    if obj.get(keys::OBJECT).and_then(|v| v.as_str()) != Some(OBJ_CHUNK) {
        return None;
    }

    if !obj.contains_key(keys::USAGE) {
        return None;
    }
    let usage = obj.remove(keys::USAGE)?;

    let mut trailing = serde_json::Map::new();
    if let Some(id) = obj.get(keys::ID) {
        trailing.insert(keys::ID.to_string(), id.clone());
    }
    trailing.insert(
        keys::OBJECT.to_string(),
        serde_json::Value::String(OBJ_CHUNK.to_string()),
    );
    if let Some(created) = obj.get(CREATED) {
        trailing.insert(CREATED.to_string(), created.clone());
    }
    if let Some(model) = obj.get(keys::MODEL) {
        trailing.insert(keys::MODEL.to_string(), model.clone());
    }
    trailing.insert(CHOICES.to_string(), serde_json::Value::Array(Vec::new()));
    trailing.insert(keys::USAGE.to_string(), usage);
    Some(serde_json::Value::Object(trailing))
}

/// Remove a folded top-level `usage` object from a `chat.completion.chunk` in place, emitting no
/// trailing chunk.
fn strip_folded_usage(chunk: &mut serde_json::Value) {
    let Some(obj) = chunk.as_object_mut() else {
        return;
    };
    if obj.get(keys::OBJECT).and_then(|v| v.as_str()) != Some(OBJ_CHUNK) {
        return;
    }
    obj.remove(keys::USAGE);
}

/// OpenAI writer implementation.
#[derive(Clone)]
pub struct OpenAiWriter {
    stream: StreamCell<OpenAiStream>,
}

/// An OpenAI chat stream's per-stream state (see [`OpenAiWriter`]).
#[derive(Clone)]
struct OpenAiStream {
    /// This stream's chunk `id`, minted once and replayed on every later opening chunk. The
    /// Anthropic reader emits `MessageStart` 1:1 with the upstream frame, so an openai-egress stream
    /// can see two; a native stream repeats one id on every chunk and the SDKs latch the first, so
    /// the first wins.
    ///
    /// `Mutex` (not `Cell`) keeps the writer `Sync` as `ProtocolWriter` requires; contention is nil,
    /// and a poisoned lock degrades to minting fresh rather than panicking.
    chunk_id: Option<String>,
    /// IR indices of this stream's text blocks that opened as a refusal (`BlockStart{refusal:
    /// true}`). Their deltas are written as `delta.refusal`, not `delta.content`; the wire has no
    /// block frame to carry the flag, so the writer remembers it.
    refusal_blocks: std::collections::BTreeSet<usize>,
}

/// Value-namespace constructor for [`OpenAiWriter`], like the sibling writers' consts: a `const`
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const OpenAiWriter: OpenAiWriter = OpenAiWriter {
    stream: StreamCell::new(OpenAiStream {
        chunk_id: None,
        refusal_blocks: std::collections::BTreeSet::new(),
    }),
};

#[cfg(test)]
include!("tests/mod_openai_writer_helper.rs");

impl OpenAiWriter {
    /// The stream's chunk `id`, first wins: the id already captured, else `mint()` captured and
    /// returned, so a duplicate opening chunk re-states what the client latched.
    fn carried_chunk_id(
        &self,
        mint: impl FnOnce() -> Result<String, IrError>,
    ) -> Result<String, IrError> {
        let mut stream = self.stream.lock();
        if let Some(id) = &stream.chunk_id {
            return Ok(id.clone());
        }
        let id = mint()?;
        stream.chunk_id = Some(id.clone());
        Ok(id)
    }

    /// Remember that the text block at `index` is a refusal (IR-02).
    fn mark_refusal_block(&self, index: usize) {
        self.stream.lock().refusal_blocks.insert(index);
    }

    /// Is the text block at `index` a refusal? A poisoned lock reads as "no" (ordinary content).
    fn is_refusal_block(&self, index: usize) -> bool {
        self.stream.lock().refusal_blocks.contains(&index)
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/input_hardening_tests.rs"]
mod input_hardening_tests;

#[cfg(test)]
#[path = "tests/audio_format_regression_tests.rs"]
mod audio_format_regression_tests;

#[cfg(test)]
#[path = "tests/field_carry_tests.rs"]
mod field_carry_tests;

#[cfg(test)]
#[path = "tests/float_usage_tests.rs"]
mod float_usage_tests;

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

/// A Chat completion's `moderation.{input,output}` results -> the IR's safety verdicts: one verdict per category a result flags (`categories.<name> = true`), `blocked` = false (a
/// moderation result reports, it does not block).
fn read_moderation(
    moderation: Option<&serde_json::Value>,
) -> Vec<crate::codec::ir::IrSafetyVerdict> {
    let mut out = Vec::new();
    for side in [MODERATION_INPUT, MODERATION_OUTPUT] {
        let results = moderation
            .and_then(|m| m.get(side))
            .and_then(|s| s.get(MODERATION_RESULTS))
            .and_then(|r| r.as_array());
        for r in results.into_iter().flatten() {
            out.extend(crate::codec::ir::IrSafetyVerdict::flagged_categories(
                r.get(CATEGORIES),
            ));
        }
    }
    out
}

const MODERATION_INPUT: &str = "input";
const MODERATION_OUTPUT: &str = "output";
const MODERATION_RESULTS: &str = "results";

/// A Chat `message.audio` -> the IR's audio output: its base64 `data` and its
/// `transcript`.
fn read_message_audio(
    audio: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrAudioOutput> {
    let audio = audio?.as_object()?;
    let text = |k: &str| audio.get(k).and_then(|v| v.as_str()).map(String::from);
    let out = crate::codec::ir::IrAudioOutput {
        data: text(keys::DATA),
        format: None,
        transcript: text(TRANSCRIPT),
    };
    (out.data.is_some() || out.transcript.is_some()).then_some(out)
}

/// The spoken text of a Chat `message.audio`.
const TRANSCRIPT: &str = "transcript";

#[cfg(test)]
#[path = "tests/audit_tool_controls_tests.rs"]
mod audit_tool_controls_tests;
