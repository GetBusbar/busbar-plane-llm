// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Gemini protocol reader/writer implementation.

use crate::codec::dialect::*;
use crate::codec::framework::ERR_TYPE_OVERLOADED;
use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use crate::codec::protocol::*;
use crate::codec::protocol::{
    ERR_TYPE_AUTHENTICATION, ERR_TYPE_INVALID_REQUEST, ERR_TYPE_NOT_FOUND, ERR_TYPE_PERMISSION,
    ERR_TYPE_RATE_LIMIT,
};
use busbar_contract::http::StatusCode;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;

#[allow(unused_imports)]
use super::proto_codec::*;

#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

mod citations;
mod framer;
pub mod handler;
#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
mod reader;
mod schema;
mod slots;
mod usage;
mod writer;

pub(crate) use citations::GeminiCitationState;
use citations::*;
pub use framer::GeminiJsonArrayFramer;
use schema::*;
use slots::*;
use usage::*;

/// The query pair a Gemini request carries to ask for event-framed chunks, written on the far-end
/// stream path and read at arrival.
pub const STREAM_QUERY: (&str, &str) = ("alt", "sse");

/// Build this dialect's wire codec. A fresh instance per resolution.
pub fn protocol() -> Protocol {
    Protocol::new(COUNT_LABEL, GeminiReader, GeminiWriter)
}

/// The [`ProtocolDecl::models_list_envelope`] builder: Gemini's `GET /v1(beta)/models` shape. Each
/// name becomes a `Model` resource (`models/{id}`, with the two generation methods busbar serves),
/// wrapped in the `{ "models": [...] }` envelope the SDK expects.
fn models_list_envelope(names: &[&str]) -> serde_json::Value {
    let models: Vec<serde_json::Value> = names
        .iter()
        .map(|id| {
            serde_json::json!({
                (keys::NAME): format!("models/{id}"),
                "displayName": id,
                "supportedGenerationMethods": [FIELD_GENERATE_CONTENT, FIELD_STREAM_GENERATE_CONTENT]
            })
        })
        .collect();
    serde_json::json!({ "models": models })
}

/// Router detection, stated here so core folds it without naming Gemini: the unique `x-goog-api-key`
fn claims(
    h: crate::codec::protocol::HeadFields<'_>,
    path: &str,
) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::framework::head_field;
    use crate::codec::protocol::ClaimStrength;
    if head_field(h, FIELD_X_GOOG_API_KEY).is_some() {
        return Some(ClaimStrength(3));
    }
    if path.contains(":generateContent")
        || path.contains(":streamGenerateContent")
        || path.contains(":embedContent")
        || path.contains(":batchEmbedContents")
        || path.contains(":predict")
    {
        return Some(ClaimStrength(5));
    }
    if path.starts_with("/v1/models/") || path.starts_with("/v1beta/models/") {
        return Some(ClaimStrength(6));
    }
    None
}

/// Gemini action suffixes the residual classifier recognises on the shared `/v1/models/{id}`
const GEMINI_RESIDUAL_ACTIONS: [&str; 7] = [
    ":generateContent",
    ":streamGenerateContent",
    ":countTokens",
    ":embedContent",
    ":batchGenerateContent",
    ":generateAnswer",
    ":batchEmbedContents",
];

/// Headerless residual detection: the whole `/v1beta/models…` surface is Gemini-only (rung 10), and
/// a `/v1/models/{id}` whose last segment carries a Gemini action suffix is Gemini (rung 20,
/// tighter than the OpenAI `/v1/models/` catch at rung 25).
fn residual_claims(path: &str) -> Option<crate::codec::protocol::ClaimStrength> {
    use crate::codec::protocol::ClaimStrength;
    if path.starts_with("/v1beta/models") {
        return Some(ClaimStrength(10));
    }
    if path.starts_with("/v1/models/") {
        let last_segment = path.rsplit('/').next().unwrap_or("");
        if GEMINI_RESIDUAL_ACTIONS
            .iter()
            .any(|a| last_segment.ends_with(a))
        {
            return Some(ClaimStrength(20));
        }
    }
    None
}

/// Gemini's declaration. The only protocol declaring an array-stream shim key, a declaration rather
/// than a literal so `proxy` removes every declared shim key without naming one.
pub const DECL: ProtocolDecl = ProtocolDecl {
    name: COUNT_LABEL,
    codec: dialect_codec!(COUNT_LABEL),
    handler: Some(&handler::GeminiRequestHandler),
    verbs: &[
        busbar_contract::operation::OpVerb::CHAT,
        busbar_contract::operation::OpVerb::EMBEDDINGS,
        busbar_contract::operation::OpVerb::IMAGE,
        busbar_contract::operation::OpVerb::TRANSCRIPTION,
        busbar_contract::operation::OpVerb::SPEECH,
    ],
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    streaming_content_type: Some(crate::codec::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: Some(GEMINI_JSON_ARRAY_SHIM_KEY),

    native_tool_id_prefix: None,
    ingress_auth: IngressAuth::Bearer,

    egress_auth_headers: None,
    egress_auth_lane_constant: false,
    egress_scheme: Some(EgressScheme::header(FIELD_X_GOOG_API_KEY)),

    stream_usage_requires_opt_in: false,
    requires_max_tokens: false,
    stop_sequence_cap: Some((5, "Gemini")),
    cache_markers_model_gated: false,
    fills_thought_signature: true,
    frame_after_message_start: None,
    reshapes_body_at_path_base: false,
    max_cache_control_breakpoints: None,
    quota_exceeded_status: busbar_contract::http::StatusCode::TOO_MANY_REQUESTS.as_u16(),
    ingress_is_eventstream: false,
    emits_sse_done_terminator: false,
    max_citations_per_delta: None,

    egress_user_agent: "google-genai-sdk/0.8.0 gl-python/3.11",
    has_model_in_url: true,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::BAD_REQUEST.as_u16(),
        ERR_TYPE_INVALID_REQUEST,
    ),
    ingress_relays_amzn_headers: false,
    ingress_relayed_response_header_names: &[],
    auth_failure_message: GEMINI_BAD_KEY_MESSAGE,
    uses_array_stream_shim: true,
    has_native_path_not_found: true,
    egress_stream_accept: crate::codec::protocol::TEXT_EVENT_STREAM,
    models_list_envelope: Some(models_list_envelope),
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: Some(vendor_response_metadata),

    list_models_fingerprint_headers: &[FIELD_X_GOOG_API_KEY],
    static_headers: &[],
};

/// This dialect's registration (its one line is in `crate::codec::DIALECTS`).
pub(crate) const ENTRY: super::proto_codec::DialectEntry = super::proto_codec::DialectEntry {
    decl: &DECL,
    protocol,
    with_writer: |f| {
        let w = GeminiWriter;
        f(&w)
    },
    with_reader: |f| f(&GeminiReader),
    leaf: &handler::LEAF,
};

/// Response-side untranslatable metadata: `safetyRatings` under `candidates[].safetyRatings`, present
/// only when requested.
fn vendor_response_metadata(body: &serde_json::Value) -> Vec<&'static str> {
    [(FIELD_SAFETY_RATINGS, "candidates[].safetyRatings")]
        .into_iter()
        .filter(|(k, _)| {
            body.get(FIELD_CANDIDATES)
                .and_then(|c| c.as_array())
                .is_some_and(|cands| cands.iter().any(|c| c.get(k).is_some()))
        })
        .map(|(_, path)| path)
        .collect()
}

const FIELD_RESPONSE_JSON_SCHEMA: &str = "responseJsonSchema";

/// The part kinds the request reader models; a `thought` part is a text part.
const REQUEST_PART_KINDS: &[&str] = &[
    keys::TEXT,
    FIELD_FUNCTION_CALL,
    FIELD_FUNCTION_RESPONSE,
    FIELD_INLINE_DATA,
    FIELD_FILE_DATA,
];

/// The members a part carries beside its kind.
const PART_COMPANIONS: &[&str] = &[FIELD_THOUGHT, FIELD_THOUGHT_SIGNATURE];

/// The Gemini request content grammar (`codec::drops`): a union keyed by its kind member. A part of
/// any other kind (`executableCode` / `codeExecutionResult`) is named as dropped on a translate attempt.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[
    crate::codec::drops::Blocks {
        at: &["contents[]", "parts[]"],
        tag: None,
        modelled: REQUEST_PART_KINDS,
        companions: PART_COMPANIONS,
    },
    crate::codec::drops::Blocks {
        at: &[FIELD_SYSTEM_INSTRUCTION, "parts[]"],
        tag: None,
        modelled: &[keys::TEXT],
        companions: PART_COMPANIONS,
    },
];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, keys::TEXT),
    (crate::codec::drops::kind::IMAGE, FIELD_INLINE_DATA),
    (crate::codec::drops::kind::DOCUMENT, FIELD_INLINE_DATA),
    (crate::codec::drops::kind::AUDIO, FIELD_INLINE_DATA),
    (crate::codec::drops::kind::VIDEO, FIELD_INLINE_DATA),
    (crate::codec::drops::kind::THINKING, FIELD_THOUGHT),
    (crate::codec::drops::kind::TOOL_USE, FIELD_FUNCTION_CALL),
    (
        crate::codec::drops::kind::TOOL_RESULT,
        FIELD_FUNCTION_RESPONSE,
    ),
];

/// IR request members the reader carries by code from a path no map-file row names.
const REQUEST_CODE_NAMES: &[(&str, &str)] = &[
    (
        crate::codec::drops::name::TOOL_CHOICE,
        "toolConfig.functionCallingConfig",
    ),
    (
        crate::codec::drops::name::RESPONSE_FORMAT,
        "generationConfig.responseSchema",
    ),
    (crate::codec::drops::name::METADATA, FIELD_LABELS),
    (
        crate::codec::drops::name::TOP_LOGPROBS,
        "generationConfig.logprobs",
    ),
    (
        crate::codec::drops::name::OUTPUT_MODALITIES,
        "generationConfig.responseModalities",
    ),
];

const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["candidates[]", "content", "parts[]"],
    tag: None,
    modelled: &[
        keys::TEXT,
        FIELD_FUNCTION_CALL,
        FIELD_INLINE_DATA,
        FIELD_FILE_DATA,
    ],
    companions: PART_COMPANIONS,
}];

/// What the Gemini reader parks in `extra` beside the members its map file does not model.
const PARKED: &[crate::codec::drops::Parked] = &[
    crate::codec::drops::Parked {
        key: keys::MODEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::STREAM,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: FIELD_LABELS,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: FIELD_GENERATION_CONFIG,
        holds: crate::codec::drops::Holds::Members(&[
            FIELD_MAX_OUTPUT_TOKENS,
            FIELD_RESPONSE_MIME_TYPE,
            FIELD_RESPONSE_SCHEMA,
            FIELD_RESPONSE_JSON_SCHEMA,
            FIELD_RESPONSE_LOGPROBS,
            keys::LOGPROBS,
            FIELD_THINKING_CONFIG,
            FIELD_RESPONSE_MODALITIES,
        ]),
    },
    crate::codec::drops::Parked {
        key: keys::TOOL_CONFIG,
        holds: crate::codec::drops::Holds::Members(&[FIELD_FUNCTION_CALLING_CONFIG]),
    },
];

/// What this dialect's answers carry beyond its map file's rows (drop walk, design F3 "Drops").
const RESPONSE_CODE: &[&str] = &["usageMetadata.candidatesTokensDetails"];

/// Answer paths inside a carried subtree that its code does not carry (where a coarse map row covers them).
const RESPONSE_DROPS: &[&str] = &[
    "candidates[].citationMetadata.citationSources[].license",
    "candidates[].content.parts[].audioTranscription",
    "candidates[].content.parts[].codeExecutionResult",
    "candidates[].content.parts[].executableCode",
    "candidates[].content.parts[].functionResponse.scheduling",
    "candidates[].content.parts[].functionResponse.willContinue",
    "candidates[].content.parts[].mediaProcessing",
    "candidates[].content.parts[].mediaResolution",
    "candidates[].content.parts[].partMetadata",
    "candidates[].content.parts[].speechMetadata",
    "candidates[].content.parts[].toolCall",
    "candidates[].content.parts[].toolResponse",
    "candidates[].content.parts[].videoMetadata",
    "candidates[].groundingMetadata.googleMapsWidgetContextToken",
    "candidates[].groundingMetadata.groundingChunks[].image.imageUri",
    "candidates[].groundingMetadata.groundingChunks[].image.sourceUri",
    "candidates[].groundingMetadata.groundingChunks[].maps",
    "candidates[].groundingMetadata.groundingChunks[].retrievedContext.customMetadata",
    "candidates[].groundingMetadata.groundingChunks[].retrievedContext.fileSearchStore",
    "candidates[].groundingMetadata.groundingChunks[].retrievedContext.mediaId",
    "candidates[].groundingMetadata.groundingChunks[].retrievedContext.pageNumber",
    "candidates[].groundingMetadata.groundingSupports[].confidenceScores",
    "candidates[].groundingMetadata.groundingSupports[].renderedParts",
    "candidates[].groundingMetadata.groundingSupports[].segment.partIndex",
    "candidates[].groundingMetadata.imageSearchQueries",
    "candidates[].groundingMetadata.retrievalMetadata",
    "candidates[].groundingMetadata.searchEntryPoint",
    "candidates[].groundingMetadata.webSearchQueries",
    "candidates[].logprobsResult.chosenCandidates[].tokenId",
    "candidates[].logprobsResult.logProbabilitySum",
    "candidates[].logprobsResult.topCandidates[].candidates[].tokenId",
];

/// Router-internal shim key the ingress route injects when the client sent a streaming
/// `:streamGenerateContent` request without `?alt=sse` (so the response must be JSON-array, not
/// SSE).
pub const GEMINI_JSON_ARRAY_SHIM_KEY: &str = "__busbar_gemini_json_array";

/// The canonical Gemini bad-API-key message (`google.rpc.Status.message` of a real 400/INVALID_ARGUMENT).
pub const GEMINI_BAD_KEY_MESSAGE: &str = "API key not valid. Please pass a valid API key.";

/// Cap on distinct tool-call block indices in `state.open_tools` for one SSE stream. The set drains
/// on either terminal path (a `finishReason` chunk, or a prompt-block envelope
/// `candidates_absent` + `promptFeedback.blockReason`), so an upstream streaming unbounded
/// `functionCall` parts without reaching one would grow it until OOM.
const MAX_GEMINI_TOOL_FRAMES: usize = 4096;

/// Gemini `FinishReason.STOP` — normal/tool-call end.
const GEMINI_FINISH_STOP: &str = "STOP";
/// Gemini `FinishReason.MAX_TOKENS` — output truncated by token cap.
const GEMINI_FINISH_MAX_TOKENS: &str = "MAX_TOKENS";
/// Gemini `FinishReason.SAFETY` — content-safety stop.
const GEMINI_FINISH_SAFETY: &str = "SAFETY";
/// Gemini `FinishReason.OTHER` — unenumerated stop reason.
const GEMINI_FINISH_OTHER: &str = "OTHER";
/// Gemini `FinishReason.MALFORMED_FUNCTION_CALL` — model produced an unparseable tool call.
const GEMINI_FINISH_MALFORMED_FUNCTION_CALL: &str = "MALFORMED_FUNCTION_CALL";
/// Gemini `FinishReason.RECITATION` — verbatim recitation stop (maps to `safety` in the IR).
const GEMINI_FINISH_RECITATION: &str = "RECITATION";
/// Gemini `FinishReason.PROHIBITED_CONTENT` — content-policy block (maps to `safety`).
const GEMINI_FINISH_PROHIBITED_CONTENT: &str = "PROHIBITED_CONTENT";

/// Upstream URL path prefix shared by all Gemini Generative Language API endpoints; the per-request
/// path appends `/{model}:{method}` (and optionally `?alt=sse`) via `upstream_path_for` /
/// `upstream_path_for_stream`.
const GEMINI_PATH_BASE: &str = "/v1beta/models";

/// JSON key for Gemini's top-level usage wrapper (`usageMetadata`).
const FIELD_USAGE_METADATA: &str = "usageMetadata";
/// JSON key for the prompt (input) token count inside `usageMetadata`.
const FIELD_PROMPT_TOKEN_COUNT: &str = "promptTokenCount";
/// JSON key for the candidates (output) token count inside `usageMetadata`.
const FIELD_CANDIDATES_TOKEN_COUNT: &str = "candidatesTokenCount";
/// JSON key for the total token count inside `usageMetadata`.
const FIELD_TOTAL_TOKEN_COUNT: &str = "totalTokenCount";
/// Key for the thinking token count in `usageMetadata` (2.5-series models). Unlike OpenAI's
/// `reasoning_tokens` it is not a subset of the visible output count but a separate additive term;
/// see [`GEMINI_USAGE_ADDITIVE_TERMS`].
const FIELD_THOUGHTS_TOKEN_COUNT: &str = "thoughtsTokenCount";
/// JSON key for the context-cache token count inside `usageMetadata`.
const FIELD_CACHED_CONTENT_TOKEN_COUNT: &str = "cachedContentTokenCount";
/// Key for the server-side tool-use prompt tokens in `usageMetadata`. Despite the name it is not a
/// slice of `promptTokenCount` but a fourth additive term beside it, charged at the input rate and
/// billed since 1.6.0 (see [`GEMINI_USAGE_ADDITIVE_TERMS`]).
const FIELD_TOOL_USE_PROMPT_TOKEN_COUNT: &str = "toolUsePromptTokenCount";
/// Key for the billing-lane marker in `usageMetadata` (`ON_DEMAND` / `PROVISIONED`). Informational,
/// not billed (see [`crate::codec::ir::types::IrUsageDetail::traffic_type`]).
const FIELD_TRAFFIC_TYPE: &str = "trafficType";

/// The measured Gemini usage identity: the four `usageMetadata` counters that are additive terms of
/// `totalTokenCount`:
///
/// ```text
/// totalTokenCount == promptTokenCount
///                  + candidatesTokenCount
///                  + thoughtsTokenCount
///                  + toolUsePromptTokenCount
/// ```
///
/// Derived from seven real Vertex AI `gemini-2.5-flash` turns under `src/tests/proto/golden/vendor/`
/// (see that directory's README). `gemini_usage_identity_tests.rs` replays every one through the
/// real decoder, so changing this list without a supporting recording turns the corpus red.
///
/// It had to be measured, since the spec left two terms ambiguous:
///
/// * `thoughtsTokenCount` — additive. Not a slice of `candidatesTokenCount`.
/// * `toolUsePromptTokenCount` — additive. The grounding recording settles it with no
///   interpretation required: `toolUsePromptTokenCount` is **32** while the whole
///   `promptTokenCount` is **18**. A sub-bucket cannot exceed its bucket.
///
/// All four terms are billed: `gemini_usage` adds the tool-use term to `input_tokens` (Google
/// charges it at the input rate) and `GeminiReader::recover_truncated_usage` does the same for a
/// response too large to reassemble; otherwise a tool-using turn would be under-counted by exactly
/// `toolUsePromptTokenCount` (32 of 222 tokens on the recording above).
///
/// The table is also the guard: [`crate::codec::usage_count::stated_total_note`] fires when Google's stated total
/// cannot be reached from it, i.e. when Google reports a counter this dialect does not model. The
/// gap is ledgered as the open class `unitemized_tokens` and the WARN names it; a recurring gap is
/// fixed by a new itemized term backed by the wire lock.
#[cfg_attr(not(test), allow(dead_code))]
const GEMINI_USAGE_ADDITIVE_TERMS: &[&str] = &[
    FIELD_PROMPT_TOKEN_COUNT,
    FIELD_CANDIDATES_TOKEN_COUNT,
    FIELD_THOUGHTS_TOKEN_COUNT,
    FIELD_TOOL_USE_PROMPT_TOKEN_COUNT,
];

/// Stable identifier for the identity [`crate::codec::usage_count::stated_total_note`] checks, carried on
/// [`crate::codec::ir::UsageIdentityNote::identity`] so callers branch on a constant.
const GEMINI_USAGE_IDENTITY: &str = "gemini.usageMetadata";

/// JSON key for the opaque response identifier emitted at the top level.
const FIELD_RESPONSE_ID: &str = "responseId";
/// JSON key for the serving model name emitted at the top level.
const FIELD_MODEL_VERSION: &str = "modelVersion";
/// Key for the RFC3339 response-creation timestamp Vertex stamps on every `GenerateContentResponse`.
const FIELD_CREATE_TIME: &str = "createTime";

/// Read Gemini/Vertex's top-level `createTime` verbatim (a foreign timestamp parser could reject a
/// Vertex precision/zone quirk).
fn read_gemini_create_time(body: &serde_json::Value) -> Option<String> {
    body.get(FIELD_CREATE_TIME)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

const GRPC_INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
const GRPC_RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
const GRPC_UNAVAILABLE: &str = "UNAVAILABLE";
const GRPC_UNAUTHENTICATED: &str = "UNAUTHENTICATED";
const GRPC_PERMISSION_DENIED: &str = "PERMISSION_DENIED";

const FIELD_ALLOWED_FUNCTION_NAMES: &str = "allowedFunctionNames";
const FIELD_ARGS: &str = "args";
const FIELD_ASPECT_RATIO: &str = "aspectRatio";
const FIELD_BYTES_BASE64_ENCODED: &str = "bytesBase64Encoded";
const FIELD_CANDIDATES: &str = "candidates";
const FIELD_CHOSEN_CANDIDATES: &str = "chosenCandidates";
const FIELD_CITATION_METADATA: &str = "citationMetadata";
const FIELD_CITATION_SOURCES: &str = "citationSources";
const FIELD_CONTENTS: &str = "contents";
const FIELD_DETAILS: &str = "details";
const FIELD_END_INDEX: &str = "endIndex";
const FIELD_FILE_DATA: &str = "fileData";
const FIELD_FILE_URI: &str = "fileUri";
const FIELD_FUNCTION_CALLING_CONFIG: &str = "functionCallingConfig";
const FIELD_FUNCTION_DECLARATIONS: &str = "functionDeclarations";
const FIELD_FUNCTION_RESPONSE: &str = "functionResponse";
const FIELD_GENERATE_CONTENT: &str = "generateContent";
const FIELD_GENERATION_CONFIG: &str = "generationConfig";
const FIELD_GUIDANCE_SCALE: &str = "guidanceScale";
const FIELD_INLINE_DATA_SNAKE: &str = "inline_data";
const FIELD_INLINE_DATA: &str = "inlineData";
const FIELD_LABELS: &str = "labels";
const FIELD_LOGPROBS_RESULT: &str = "logprobsResult";
const FIELD_LOG_PROBABILITY: &str = "logProbability";
const FIELD_MAX_OUTPUT_TOKENS: &str = "maxOutputTokens";
const FIELD_MIME_TYPE_SNAKE: &str = "mime_type";
const FIELD_MIME_TYPE: &str = "mimeType";
const FIELD_MODALITY: &str = "modality";
const FIELD_NEGATIVE_PROMPT: &str = "negativePrompt";
const FIELD_NULL: &str = "null";
const FIELD_NULLABLE: &str = "nullable";
const FIELD_OUTPUT_DIMENSIONALITY: &str = "outputDimensionality";
const FIELD_PARTS: &str = "parts";
const FIELD_PERSON_GENERATION: &str = "personGeneration";
const FIELD_PREDICTIONS: &str = "predictions";
const FIELD_RESPONSE_LOGPROBS: &str = "responseLogprobs";
const FIELD_RESPONSE_MODALITIES: &str = "responseModalities";
const FIELD_RESPONSE_SCHEMA: &str = "responseSchema";
const FIELD_RETRIEVED_CONTEXT: &str = "retrievedContext";
const FIELD_SAMPLE_COUNT: &str = "sampleCount";
const FIELD_SAMPLE_IMAGE_SIZE: &str = "sampleImageSize";
const FIELD_START_INDEX: &str = "startIndex";
const FIELD_STREAM_GENERATE_CONTENT: &str = "streamGenerateContent";
const FIELD_SYSTEM_INSTRUCTION: &str = "systemInstruction";
const FIELD_THINKING_BUDGET: &str = "thinkingBudget";
const FIELD_THINKING_CONFIG: &str = "thinkingConfig";
const FIELD_THOUGHT: &str = "thought";
const FIELD_THOUGHT_SIGNATURE: &str = "thoughtSignature";
const FIELD_TOKEN_COUNT: &str = "tokenCount";
const FIELD_TOP_CANDIDATES: &str = "topCandidates";
const FIELD_URI: &str = "uri";
const FIELD_VALUES: &str = "values";
const FIELD_X_GOOG_API_KEY: &str = "x-goog-api-key";
const GEMINI_ABORTED: &str = "ABORTED";
const GEMINI_ANY: &str = "ANY";
const GEMINI_AUDIO: &str = "AUDIO";
const GEMINI_AUTO: &str = "AUTO";
const GEMINI_BLOCKLIST: &str = "BLOCKLIST";
const GEMINI_CANCELLED: &str = "CANCELLED";
const GEMINI_IMAGE_SAFETY: &str = "IMAGE_SAFETY";
const GEMINI_UNKNOWN: &str = "UNKNOWN";
const GRPC_INTERNAL: &str = "INTERNAL";
const GRPC_DEADLINE_EXCEEDED: &str = "DEADLINE_EXCEEDED";
const GRPC_NOT_FOUND: &str = "NOT_FOUND";
const GRPC_UNIMPLEMENTED: &str = "UNIMPLEMENTED";
/// The google.rpc.Code names and the HTTP status each pairs with on the Generative Language API:
const STATUS_HTTP: &[(&str, u16)] = &[
    (GRPC_INVALID_ARGUMENT, 400),
    (GRPC_UNAUTHENTICATED, 401),
    (GRPC_PERMISSION_DENIED, 403),
    (GRPC_NOT_FOUND, 404),
    (GRPC_RESOURCE_EXHAUSTED, 429),
    (GRPC_INTERNAL, 500),
    (GRPC_UNIMPLEMENTED, 501),
    (GRPC_UNAVAILABLE, 503),
    (GRPC_DEADLINE_EXCEEDED, 504),
];

/// The machine-readable `reason` value carried in `google.rpc.ErrorInfo` for an invalid API key.
const GEMINI_ERROR_REASON_API_KEY_INVALID: &str = "API_KEY_INVALID";
/// The protobuf type URL for `google.rpc.ErrorInfo` (carried in `details[].@type`).
const GEMINI_ERROR_INFO_TYPE_URL: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// JSON key for the MIME type of the response format inside `generationConfig`.
const FIELD_RESPONSE_MIME_TYPE: &str = "responseMimeType";
/// MIME type value for JSON structured output.
const MIME_APPLICATION_JSON: &str = "application/json";
/// JSON key for a `functionCall` content part.
const FIELD_FUNCTION_CALL: &str = "functionCall";
/// JSON key for the finish reason on a candidate.
const FIELD_FINISH_REASON: &str = "finishReason";

#[derive(Clone)]
pub struct GeminiReader;

/// Base62 alphabet for the synthesized `responseId` (shared atom `crate::codec::dialect::BASE62_ALPHABET`).
const RESPONSE_ID_ALPHABET: &[u8; 62] = crate::codec::dialect::BASE62_ALPHABET;

/// Width of a synthesized `responseId`. Native ids are short opaque tokens (~11-16 chars) with no
/// structure; 16 base62 chars stays in that profile so a length- or regex-checking client cannot
/// fingerprint it as non-native.
const RESPONSE_ID_TOKEN_LEN: usize = 16;

/// Rejection threshold for the base62 reduction in `synth_response_id`: the largest multiple of 62
/// in a `u8` is 248, so a byte `>= 248` is rejected and resampled to avoid a biased `byte % 62`.
const RESPONSE_ID_REJECT_THRESHOLD: u8 = crate::codec::dialect::BASE62_REJECT_THRESHOLD;

/// Mint a Gemini-shaped `responseId` where the backend supplied none (cross-protocol). A native id
/// is an opaque mixed-case alphanumeric token with no hyphen, hex-only class or embedded timestamp;
/// a `{:x}-{:x}` timestamp-and-counter form was distinguishable and leaked the host clock.
fn synth_response_id(host: &dyn crate::codec::cells::CodecHost) -> Option<String> {
    let mut token = [b'0'; RESPONSE_ID_TOKEN_LEN];
    let mut filled = 0usize;

    let mut rounds = 0u32;
    const MAX_ROUNDS: u32 = 8;
    while filled < RESPONSE_ID_TOKEN_LEN && rounds < MAX_ROUNDS {
        rounds += 1;

        let mut batch = [0u8; RESPONSE_ID_TOKEN_LEN * 2];
        if !host.fill_entropy(&mut batch) {
            return None;
        }
        for &byte in batch.iter() {
            if filled >= RESPONSE_ID_TOKEN_LEN {
                break;
            }
            if byte >= RESPONSE_ID_REJECT_THRESHOLD {
                continue;
            }
            token[filled] = RESPONSE_ID_ALPHABET[(byte % 62) as usize];
            filled += 1;
        }
    }

    if filled != RESPONSE_ID_TOKEN_LEN {
        return None;
    }

    String::from_utf8(token.to_vec()).ok()
}

/// Synthesize a stable, non-empty tool-call id for a Gemini `functionCall`. The wire carries none,
/// and an empty id on Anthropic/OpenAI egress (which require one to correlate the later
/// `tool_result`) made same-named calls indistinguishable and broke result routing.
fn synth_tool_call_id(call_index: usize, function_name: &str, turn_salt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_index.hash(&mut hasher);
    function_name.hash(&mut hasher);
    turn_salt.hash(&mut hasher);
    format!("call_{:016x}", hasher.finish())
}

/// The native call id on a `functionCall` / `functionResponse` object (IR audit GEM-08). Both `id`s
/// are optional; when populated, the response is paired with its call by it.
fn gemini_call_id(obj: &serde_json::Value) -> Option<&str> {
    obj.get(keys::ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Pairs each `functionResponse` in a request history with the `functionCall` it answers (IR audit
/// GEM-01). Anthropic and OpenAI reject a history whose result names no earlier call. Gemini keys
/// the pair by the call's native `id` when both carry one, else by position: a user turn's
/// responses answer the previous model turn's calls in order. The reader once gave calls a
/// synthesized `call_<hash>` and results the bare name, so no pair matched on a foreign target.
///
/// Resolution for one response: (1) its own `id`; (2) else the earliest still-unanswered call of
/// the same name in the most recent model turn that has one (positional pairing for parallel
/// same-name calls; a stale call in an older turn never steals a newer turn's answer); (3) else the
/// name, as before: a result with no call to pair with stays an orphan and is warned about.
type GeminiNamedCalls = std::collections::BTreeSet<(usize, std::cmp::Reverse<usize>)>;

#[derive(Default)]
struct GeminiCallLedger {
    calls: Vec<(String, String, usize)>,
    by_id: std::collections::BTreeMap<String, std::collections::BTreeSet<usize>>,
    by_name: std::collections::BTreeMap<String, GeminiNamedCalls>,
}

impl GeminiCallLedger {
    fn record_call(&mut self, id: &str, name: &str, turn: usize) {
        let index = self.calls.len();
        self.calls.push((id.to_string(), name.to_string(), turn));
        self.by_id.entry(id.to_string()).or_default().insert(index);
        self.by_name
            .entry(name.to_string())
            .or_default()
            .insert((turn, std::cmp::Reverse(index)));
    }

    fn answer(&mut self, index: usize) -> String {
        let (id, name, turn) = &self.calls[index];
        if let Some(indices) = self.by_id.get_mut(id) {
            indices.remove(&index);
        }
        if let Some(indices) = self.by_name.get_mut(name) {
            indices.remove(&(*turn, std::cmp::Reverse(index)));
        }
        id.clone()
    }

    fn pair_response(&mut self, explicit_id: Option<&str>, name: &str) -> String {
        if let Some(id) = explicit_id {
            if let Some(index) = self
                .by_id
                .get(id)
                .and_then(|indices| indices.first())
                .copied()
            {
                self.answer(index);
            }
            return id.to_string();
        }
        if let Some((_, std::cmp::Reverse(index))) = self
            .by_name
            .get(name)
            .and_then(|indices| indices.last())
            .copied()
        {
            return self.answer(index);
        }
        tracing::warn!(
            tool_name = %name,
            "gemini functionResponse answers no unanswered functionCall of that name in the \
             history; it is carried with the function name as its tool_use_id and a foreign \
             backend sees an orphan tool result"
        );
        name.to_string()
    }
}

/// Read one Gemini attachment part (`inlineData{mimeType,data}` or `fileData{fileUri,mimeType}`)
/// into its IR block.
fn read_gemini_media_part(part: &serde_json::Value) -> Option<crate::codec::ir::IrBlock> {
    if let Some(inline_data) = part.get(FIELD_INLINE_DATA) {
        let mime_type = inline_data
            .get(FIELD_MIME_TYPE)
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let data = inline_data
            .get(keys::DATA)
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        return Some(
            if mime_type.to_ascii_lowercase().starts_with(keys::IMAGE_) {
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Base64 {
                        media_type: mime_type,
                        data,
                    },
                    cache_control: None,
                    detail: None,
                }
            } else {
                crate::codec::ir::IrBlock::Media {
                    kind: crate::codec::ir::IrMediaKind::from_media_type(&mime_type),
                    source: crate::codec::ir::IrImageSource::Base64 {
                        media_type: mime_type,
                        data,
                    },
                    name: None,
                    cache_control: None,
                    citations: None,
                    context: None,
                }
            },
        );
    }
    if let Some(file_data) = part.get(FIELD_FILE_DATA) {
        let uri = file_data
            .get(FIELD_FILE_URI)
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        let mime = file_data
            .get(FIELD_MIME_TYPE)
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let source = if mime.is_empty() {
            crate::codec::ir::IrImageSource::Url(uri)
        } else {
            crate::codec::ir::IrImageSource::UrlWithMime {
                url: uri,
                media_type: mime.to_string(),
            }
        };
        return Some(
            if mime.is_empty() || mime.to_ascii_lowercase().starts_with(keys::IMAGE_) {
                crate::codec::ir::IrBlock::Image {
                    source,
                    cache_control: None,
                    detail: None,
                }
            } else {
                crate::codec::ir::IrBlock::Media {
                    kind: crate::codec::ir::IrMediaKind::from_media_type(mime),
                    source,
                    name: None,
                    cache_control: None,
                    citations: None,
                    context: None,
                }
            },
        );
    }
    None
}

/// Write one IR attachment block as its Gemini part (`inlineData` / `fileData`); inverse of
/// [`read_gemini_media_part`], used for a tool result's attachments (`functionResponse.parts`,
/// GEM-06).
fn write_gemini_media_part(block: &crate::codec::ir::IrBlock) -> Option<serde_json::Value> {
    let (source, url_mime): (&crate::codec::ir::IrImageSource, &str) = match block {
        crate::codec::ir::IrBlock::Image { source, .. } => (
            source,
            match source {
                crate::codec::ir::IrImageSource::Url(u) => gemini_image_mime_for_url(u),
                _ => "",
            },
        ),
        crate::codec::ir::IrBlock::Media { kind, source, .. } => {
            (source, gemini_mime_for_kind(*kind))
        }
        _ => return None,
    };
    match source {
        crate::codec::ir::IrImageSource::UrlWithMime { url, media_type } => {
            Some(serde_json::json!({
                (FIELD_FILE_DATA): { (FIELD_FILE_URI): url, (FIELD_MIME_TYPE): media_type }
            }))
        }
        crate::codec::ir::IrImageSource::Url(uri) => Some(serde_json::json!({
            (FIELD_FILE_DATA): { (FIELD_FILE_URI): uri, (FIELD_MIME_TYPE): url_mime }
        })),
        crate::codec::ir::IrImageSource::Base64 { media_type, data } => Some(serde_json::json!({
            (FIELD_INLINE_DATA): { (FIELD_MIME_TYPE): media_type, (keys::DATA): data }
        })),
        crate::codec::ir::IrImageSource::Vendor { .. } => None,
    }
}

/// A representative `mimeType` for a media kind whose source is a bare URL with no mime of its own.
fn gemini_mime_for_kind(kind: crate::codec::ir::IrMediaKind) -> &'static str {
    match kind {
        crate::codec::ir::IrMediaKind::Document => "application/pdf",
        crate::codec::ir::IrMediaKind::Audio => keys::AUDIO_MPEG,
        crate::codec::ir::IrMediaKind::Video => "video/mp4",
    }
}

/// A representative `image/*` `mimeType` for a bare-URL image (`fileData` requires one, see above).
fn gemini_image_mime_for_url(uri: &str) -> &'static str {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    const GEMINI_IMAGE_TYPES: &[(&str, &str)] = &[("heic", "image/heic"), ("heif", "image/heif")];
    crate::codec::dialect::media_type(
        &[crate::codec::dialect::IMAGE_MEDIA_TYPES, GEMINI_IMAGE_TYPES],
        &ext,
    )
    .unwrap_or("image/jpeg")
}

/// `thinkingConfig.thinkingLevel` (Gemini 3) to the IR effort ask (IR audit GEM-09). The same concept
/// as OpenAI's `reasoning_effort`, so one-for-one onto [`crate::codec::ir::IrReasoningEffort`].
fn read_gemini_thinking_level(level: &str) -> Option<crate::codec::ir::IrReasoningAsk> {
    let effort = match level.to_ascii_lowercase().as_str() {
        "minimal" => crate::codec::ir::IrReasoningEffort::Minimal,
        "low" => crate::codec::ir::IrReasoningEffort::Low,
        "medium" => crate::codec::ir::IrReasoningEffort::Medium,
        "high" => crate::codec::ir::IrReasoningEffort::High,
        _ => return None,
    };
    Some(crate::codec::ir::IrReasoningAsk::Effort(effort))
}

/// Read `thinkingConfig.thinkingBudget` as Gemini's API does (proto3 JSON int32: a whole number or a
/// decimal string): `-1` ("the model decides"), `0` (off) or a positive count.
fn read_gemini_thinking_budget(v: &serde_json::Value) -> Option<i32> {
    let n = match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => i,
            None => {
                let f = n.as_f64()?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return None;
                }

                f as i64
            }
        },
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    i32::try_from(n).ok().filter(|n| *n >= -1)
}

/// Normalize a Gemini OpenAPI-subset `Schema` (`parameters`, `responseSchema`) into JSON Schema for
/// the IR (IR audit GEM-11).
fn gemini_openapi_schema_to_json_schema(schema: &serde_json::Value) -> serde_json::Value {
    fn walk(v: &serde_json::Value, depth: usize) -> serde_json::Value {
        let Some(obj) = v.as_object() else {
            return v.clone();
        };
        if depth > GEMINI_SCHEMA_INLINE_MAX_DEPTH {
            return v.clone();
        }
        let mut out = serde_json::Map::new();
        for (k, val) in obj {
            let mapped = match k.as_str() {
                keys::TYPE => match val.as_str() {
                    Some(t) => match t {
                        "STRING" | "NUMBER" | "INTEGER" | "BOOLEAN" | "ARRAY" | "OBJECT"
                        | "NULL" => serde_json::json!(t.to_ascii_lowercase()),
                        _ => val.clone(),
                    },
                    None => val.clone(),
                },
                keys::PROPERTIES | "$defs" | keys::DEFINITIONS => match val.as_object() {
                    Some(m) => serde_json::Value::Object(
                        m.iter()
                            .map(|(pk, pv)| (pk.clone(), walk(pv, depth + 1)))
                            .collect(),
                    ),
                    None => val.clone(),
                },
                "items" | keys::ADDITIONAL_PROPERTIES => walk(val, depth + 1),
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => match val.as_array() {
                    Some(a) => {
                        serde_json::Value::Array(a.iter().map(|s| walk(s, depth + 1)).collect())
                    }
                    None => val.clone(),
                },
                _ => val.clone(),
            };
            out.insert(k.clone(), mapped);
        }

        if let Some(nullable) = out.get(FIELD_NULLABLE).and_then(|n| n.as_bool()) {
            match out
                .get(keys::TYPE)
                .and_then(|t| t.as_str())
                .map(str::to_string)
            {
                Some(t) => {
                    out.remove(FIELD_NULLABLE);
                    if nullable && t != FIELD_NULL {
                        out.insert(keys::TYPE.to_string(), serde_json::json!([t, FIELD_NULL]));
                    }
                }
                None if !nullable => {
                    out.remove(FIELD_NULLABLE);
                }
                None => {}
            }
        }
        serde_json::Value::Object(out)
    }
    walk(schema, 0)
}

/// Convert Vertex's RFC 3339 `createTime` (`2025-06-01T12:34:56.123456Z`, or with a `±HH:MM` offset)
/// into Unix epoch seconds, the IR's `created`.
fn gemini_rfc3339_to_epoch(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset_secs: i64 = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b[i + 3] == b':' => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            if *sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return None,
    };

    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let epoch = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(epoch).ok()
}

/// Gemini's `logprobsResult` (parallel arrays `chosenCandidates[i]` and `topCandidates[i].candidates[]`)
/// zipped into neutral IR entries.
fn read_gemini_logprobs(v: Option<&serde_json::Value>) -> Vec<crate::codec::ir::IrTokenLogprob> {
    let chosen = match v
        .and_then(|lr| lr.get(FIELD_CHOSEN_CANDIDATES))
        .and_then(|c| c.as_array())
    {
        Some(c) => c,
        None => return Vec::new(),
    };
    let tops = v
        .and_then(|lr| lr.get(FIELD_TOP_CANDIDATES))
        .and_then(|c| c.as_array());
    chosen
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            Some(crate::codec::ir::IrTokenLogprob {
                token: c.get(keys::TOKEN)?.as_str()?.to_string(),
                logprob: c.get(FIELD_LOG_PROBABILITY)?.as_f64()?,
                bytes: None,
                bytes_null: false,
                top: tops
                    .and_then(|t| t.get(i))
                    .and_then(|t| t.get(FIELD_CANDIDATES))
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| {
                                Some(crate::codec::ir::IrTopLogprob {
                                    token: t.get(keys::TOKEN)?.as_str()?.to_string(),
                                    logprob: t.get(FIELD_LOG_PROBABILITY)?.as_f64()?,
                                    bytes: None,
                                    bytes_null: false,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Neutral IR logprobs to `logprobsResult` (chosen + top arrays). `topCandidates` is emitted only
/// when some position has alternatives, as Gemini omits it when the top-count was not requested.
fn write_gemini_logprobs_result(lps: &[crate::codec::ir::IrTokenLogprob]) -> serde_json::Value {
    let chosen: Vec<serde_json::Value> = lps
        .iter()
        .map(|lp| serde_json::json!({(keys::TOKEN): lp.token, (FIELD_LOG_PROBABILITY): lp.logprob}))
        .collect();
    let mut obj = serde_json::json!({ (FIELD_CHOSEN_CANDIDATES): chosen });
    if lps.iter().any(|lp| !lp.top.is_empty()) {
        let tops: Vec<serde_json::Value> = lps
            .iter()
            .map(|lp| {
                serde_json::json!({
                    (FIELD_CANDIDATES): lp
                        .top
                        .iter()
                        .map(|t| serde_json::json!({(keys::TOKEN): t.token, (FIELD_LOG_PROBABILITY): t.logprob}))
                        .collect::<Vec<serde_json::Value>>()
                })
            })
            .collect();
        obj[FIELD_TOP_CANDIDATES] = serde_json::json!(tops);
    }
    obj
}

/// Normalize `toolConfig.functionCallingConfig` into the IR `tool_choice`: `AUTO` to `Auto`, `NONE`
fn read_gemini_tool_choice(
    tool_config: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    let fcc = tool_config?.get(FIELD_FUNCTION_CALLING_CONFIG)?;
    let mode = fcc.get(keys::MODE).and_then(|m| m.as_str())?;
    match mode.to_uppercase().as_str() {
        GEMINI_AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
        keys::NONE_UPPER => Some(crate::codec::ir::IrToolChoice::None),
        GEMINI_ANY => {
            let names = fcc
                .get(FIELD_ALLOWED_FUNCTION_NAMES)
                .and_then(|a| a.as_array());
            match names {
                Some(arr) if arr.len() > 1 => Some(crate::codec::ir::IrToolChoice::Required),
                _ => match names.and_then(|a| a.first()).and_then(|n| n.as_str()) {
                    Some(name) => Some(crate::codec::ir::IrToolChoice::Tool {
                        name: name.to_string(),
                    }),
                    None => Some(crate::codec::ir::IrToolChoice::Required),
                },
            }
        }
        _ => None,
    }
}

/// Emit the IR `tool_choice` union as a Gemini `functionCallingConfig` object.
fn write_gemini_tool_choice(tc: &crate::codec::ir::IrToolChoice) -> serde_json::Value {
    match tc {
        crate::codec::ir::IrToolChoice::Auto => serde_json::json!({(keys::MODE): GEMINI_AUTO}),
        crate::codec::ir::IrToolChoice::None => serde_json::json!({(keys::MODE): keys::NONE_UPPER}),
        crate::codec::ir::IrToolChoice::Required => serde_json::json!({(keys::MODE): GEMINI_ANY}),
        crate::codec::ir::IrToolChoice::Tool { name } => {
            serde_json::json!({(keys::MODE): GEMINI_ANY, (FIELD_ALLOWED_FUNCTION_NAMES): [name]})
        }
    }
}

/// Default an absent `functionCall.args` to `{}`, not `null`. A zero-argument call omits `args` or
/// sends an empty object; `null` leaked `"input": null` onto Anthropic/OpenAI egress, which strict
/// SDKs reject.
fn empty_object_if_absent(args: Option<&serde_json::Value>) -> serde_json::Value {
    match args {
        Some(v) => v.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Coerce an `IrBlock::ToolUse.input` into a valid `functionCall.args` (a protobuf Struct, so it
/// must be a JSON object).
fn coerce_tool_args(input: &serde_json::Value) -> serde_json::Value {
    let candidate: serde_json::Value = match input.as_str() {
        Some(s) => crate::codec::json::parse_str(s).unwrap_or_else(|_| input.clone()),
        None => input.clone(),
    };
    if candidate.is_object() {
        candidate
    } else if candidate.is_null() {
        serde_json::json!({})
    } else {
        serde_json::json!({ (FIELD_ARGS): candidate })
    }
}

/// True when a response/stream chunk has no usable `candidates` (absent, non-array or empty),
/// distinguishing a prompt-block / error-only envelope from a candidate-bearing chunk.
fn candidates_absent(data: &serde_json::Value) -> bool {
    match data.get(FIELD_CANDIDATES).and_then(|c| c.as_array()) {
        Some(arr) => arr.is_empty(),
        None => true,
    }
}

/// A top-level `promptFeedback.blockReason` (the prompt-level block signal), e.g.
fn prompt_block_reason(data: &serde_json::Value) -> Option<&str> {
    data.get(FIELD_PROMPT_FEEDBACK)
        .and_then(|pf| pf.get("blockReason"))
        .and_then(|r| r.as_str())
        .filter(|s| !s.is_empty())
}

/// Map a candidate `finishReason` to a canonical IR stop reason. `STOP`/`MAX_TOKENS`/`SAFETY` map
/// to `end_turn`/`max_tokens`/`safety`.
fn map_gemini_finish_reason(finish_reason: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match finish_reason {
        GEMINI_FINISH_STOP => S::EndTurn,
        GEMINI_FINISH_MAX_TOKENS => S::MaxTokens,
        GEMINI_FINISH_SAFETY
        | GEMINI_FINISH_RECITATION
        | GEMINI_IMAGE_SAFETY
        | "SPII"
        | GEMINI_BLOCKLIST
        | GEMINI_FINISH_PROHIBITED_CONTENT
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" => S::Safety,

        GEMINI_FINISH_MALFORMED_FUNCTION_CALL | "UNEXPECTED_TOOL_CALL" => S::Error,

        _ => S::Other,
    }
}

/// Map a `promptFeedback.blockReason` to a canonical IR stop reason. A prompt block refuses the
/// input on content policy, so the well-known reasons surface as `safety` (as candidate-level
/// `SAFETY` does); any other reason is lowercased so a novel one is surfaced, not dropped.
fn prompt_block_stop_reason(block_reason: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match block_reason {
        GEMINI_FINISH_SAFETY
        | GEMINI_BLOCKLIST
        | GEMINI_FINISH_PROHIBITED_CONTENT
        | GEMINI_FINISH_RECITATION
        | GEMINI_IMAGE_SAFETY => S::Safety,
        _ => S::Other,
    }
}

/// IR stop reason to Gemini's native `finishReason`. Exhaustive. The enum has no TOOL_USE, so
/// EndTurn/StopSequence/ToolUse give STOP; MaxTokens gives MAX_TOKENS; Safety gives SAFETY; any
/// other gives the native `OTHER` (valid and honest, never an off-spec token).
fn write_gemini_stop_reason(reason: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match reason {
        S::EndTurn | S::StopSequence | S::ToolUse => GEMINI_FINISH_STOP,
        S::MaxTokens => GEMINI_FINISH_MAX_TOKENS,
        S::Safety => GEMINI_FINISH_SAFETY,

        S::Refusal | S::Error | S::PauseTurn | S::Other => GEMINI_FINISH_OTHER,
    }
}

/// Map a canonical `StatusClass` onto the `(HTTP code, google.rpc.Code name)` pair Gemini uses in
/// its `google.rpc.Status` error envelope.
fn gemini_stream_error_code_status(class: StatusClass) -> (u16, &'static str) {
    match class {
        StatusClass::RateLimit => (429, GRPC_RESOURCE_EXHAUSTED),
        StatusClass::Overloaded => (503, GRPC_UNAVAILABLE),
        StatusClass::ServerError => (500, GRPC_INTERNAL),
        StatusClass::Timeout => (504, GRPC_DEADLINE_EXCEEDED),
        StatusClass::Network => (503, GRPC_UNAVAILABLE),
        StatusClass::Auth => (401, GRPC_UNAUTHENTICATED),
        StatusClass::Billing => (403, GRPC_PERMISSION_DENIED),
        StatusClass::ClientError => (400, GRPC_INVALID_ARGUMENT),
        StatusClass::ContextLength => (400, GRPC_INVALID_ARGUMENT),
    }
}

/// Map an inline google.rpc.Status `(status name, code)`, as in a 200-status SSE error chunk's
/// `error` object, to a `StatusClass`.
fn gemini_error_status_class(status: Option<&str>, code: Option<u64>) -> StatusClass {
    if let Some(name) = status {
        match name {
            GRPC_RESOURCE_EXHAUSTED => return StatusClass::RateLimit,
            GRPC_UNAVAILABLE => return StatusClass::Overloaded,
            GRPC_DEADLINE_EXCEEDED => return StatusClass::Timeout,
            GRPC_UNAUTHENTICATED => return StatusClass::Auth,
            GRPC_PERMISSION_DENIED => return StatusClass::Billing,
            GRPC_INVALID_ARGUMENT
            | "FAILED_PRECONDITION"
            | "OUT_OF_RANGE"
            | GRPC_NOT_FOUND
            | "ALREADY_EXISTS"
            | GEMINI_ABORTED
            | GEMINI_CANCELLED => return StatusClass::ClientError,
            GRPC_INTERNAL | GEMINI_UNKNOWN | "DATA_LOSS" | GRPC_UNIMPLEMENTED => {
                return StatusClass::ServerError;
            }

            other => {
                let _ = other;
            }
        }
    }
    match code {
        Some(429) => StatusClass::RateLimit,
        Some(503) => StatusClass::Overloaded,
        Some(504) => StatusClass::Timeout,
        Some(401) => StatusClass::Auth,
        Some(403) => StatusClass::Billing,
        Some(c) if (400..500).contains(&c) => StatusClass::ClientError,

        Some(_) | None => StatusClass::ServerError,
    }
}

/// Gemini writer implementation.
///
/// Carries one piece of per-stream state: the open streaming tool calls. A native stream emits a
/// tool call as one `functionCall` part `{name, args}`, but the IR carries the name only on
/// `BlockStart` and the arguments on `InputJsonDelta` fragments, which a cross-protocol backend
/// (OpenAI / Anthropic) often splits (`{"lo`, `c":"SF"}`). Emitting per event produced N parts (a
/// `{name, args:{}}` frame plus a nameless unparseable `{args}` frame per fragment), a
/// split-and-data-loss shape a native google-genai client never sees.
///
/// So each open tool block is buffered (name from `BlockStart`, fragments concatenated), nothing is
/// emitted on `BlockStart` or the deltas, and `BlockStop` parses the reassembled string once and
/// emits a single `{name, args}` part. A zero-argument call flushes `{name, args:{}}`, never lost.
///
/// The buffer is a `Vec` keyed by IR block index, not one slot: a backend may open parallel tool
/// blocks (the OpenAI reader emits BlockStart(1), BlockStart(2), their deltas, then BlockStop(1),
/// BlockStop(2)), and a single slot would be clobbered by the second BlockStart.
///
/// `StreamTranslate::new` builds a fresh `Protocol::gemini()` per stream, so the state is
/// stream-scoped, as with `ResponsesWriter`'s `sequence`/`response_id`.
/// One open streaming tool call in [`GeminiWriter::open_tools`]: the IR block `index`, the call's
/// `id` (re-emitted as `functionCall.id`, GEM-08), its function `name`, and the `InputJsonDelta`
/// fragments concatenated into `args`.
#[derive(Clone, Debug)]
struct GeminiOpenTool {
    index: usize,
    id: String,
    name: String,
    args: String,
    overflowed: bool,
}

#[derive(Clone)]
pub struct GeminiWriter {
    stream: StreamCell<GeminiStream>,
}

/// A Gemini stream's per-stream state (see [`GeminiWriter`]).
#[derive(Clone)]
struct GeminiStream {
    /// The open streaming tool calls, one [`GeminiOpenTool`] per open block. `index` matches later
    /// `BlockDelta`/`BlockStop` events to the right block (parallel calls share no slot); `args` is
    /// every fragment concatenated and parsed once on `BlockStop` (empty means a zero-argument call,
    /// flushed as `args:{}`). A `Vec` keeps the common 0-2 open tools cheap; the linear scan is
    /// bounded by the upstream reader's tool-frame cap.
    open_tools: Vec<GeminiOpenTool>,
    /// This stream's `responseId`, minted once and replayed on every later identity frame. A stream
    /// can carry two `MessageStart`s (the Anthropic reader emits one per upstream frame), and a fresh
    /// id for the second changed `chunk.response_id` mid-stream in the `google-genai` SDK. A native
    /// stream's id is fixed, so the first wins.
    response_id: Option<String>,
    /// This stream's answer text so far and the byte offset each text block started at, used to
    /// convert a streamed citation's character offsets (relative to its text block, the IR
    /// contract) into the candidate-wide byte offsets `citationSources` carries (IR audit GEM-16),
    /// as the buffered writer does.
    stream_text: GeminiStreamText,
    /// This stream's translation cap: the byte ceiling one open tool's streamed `args` may reach
    /// (see the `InputJsonDelta` arm of `write_response_event`), set by the plane
    /// ([`crate::codec::proto_codec::ProtocolWriter::set_translate_cap`]); until then
    /// [`crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT`].
    translate_cap: usize,
}

/// The streamed answer text [`GeminiWriter::stream_text`] accumulates: the concatenated `text` and
/// one retained range per text block in order of first text, with the character
/// count and index marks kept as it grows so a citation converts without walking the text.
#[derive(Clone, Debug, Default)]
struct GeminiStreamText {
    text: String,
    truncated: bool,
    block_starts: std::collections::BTreeMap<usize, GeminiStreamBlock>,
    /// Bytes emitted, including those beyond the retained prefix, for contiguity detection.
    emitted_bytes: usize,
    chars: usize,
    marks: Vec<usize>,
}

#[derive(Clone, Debug)]
struct GeminiStreamBlock {
    start: usize,
    end: usize,
    contiguous: bool,
}

/// Value-namespace constructor for [`GeminiWriter`], so sites writing the bare `GeminiWriter`
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const GeminiWriter: GeminiWriter = GeminiWriter {
    stream: StreamCell::new(GeminiStream {
        open_tools: Vec::new(),
        response_id: None,
        stream_text: GeminiStreamText {
            text: String::new(),
            truncated: false,
            block_starts: std::collections::BTreeMap::new(),
            emitted_bytes: 0,
            chars: 0,
            marks: Vec::new(),
        },
        translate_cap: crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }),
};

impl GeminiWriter {
    /// Resolve entropy outside the stream lock, then publish the first successful identity.
    fn carried_response_id(&self, mint: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some(id) = self.stream.lock().response_id.clone() {
            return Some(id);
        }
        let minted = mint();
        let mut stream = self.stream.lock();
        if stream.response_id.is_none() {
            stream.response_id = minted;
        }
        stream.response_id.clone()
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/input_hardening_tests.rs"]
mod input_hardening_tests;

#[cfg(test)]
#[path = "tests/logprobs_carry_tests.rs"]
mod logprobs_carry_tests;

#[cfg(test)]
#[path = "tests/image_url_mime_regression_tests.rs"]
mod image_url_mime_regression_tests;

#[cfg(test)]
#[path = "tests/field_carry_tests.rs"]
mod field_carry_tests;

#[cfg(test)]
#[path = "tests/usage_identity_tests.rs"]
mod usage_identity_tests;

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
#[path = "tests/df_map_audit_tests.rs"]
mod df_map_audit_tests;

/// A candidate's and the prompt's `safetyRatings[]` to IR safety verdicts: the category, with
/// `blocked` as both `flagged` and `blocked`.
fn read_safety_ratings(
    body: &serde_json::Value,
    candidate: &serde_json::Value,
) -> Vec<crate::codec::ir::IrSafetyVerdict> {
    let prompt = body
        .get(FIELD_PROMPT_FEEDBACK)
        .and_then(|p| p.get(FIELD_SAFETY_RATINGS));
    [candidate.get(FIELD_SAFETY_RATINGS), prompt]
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_array())
        .flatten()
        .filter_map(|r| {
            let blocked = r
                .get(FIELD_BLOCKED)
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            Some(crate::codec::ir::IrSafetyVerdict {
                category: r.get(FIELD_CATEGORY)?.as_str()?.to_string(),
                flagged: blocked,
                blocked,
            })
        })
        .collect()
}

/// The IR safety verdicts as a candidate's `safetyRatings[]` (`{category, blocked}`, no
/// probability).
fn write_safety_ratings(safety: &[crate::codec::ir::IrSafetyVerdict]) -> Option<serde_json::Value> {
    (!safety.is_empty()).then(|| {
        serde_json::Value::Array(
            safety
                .iter()
                .map(|v| serde_json::json!({ (FIELD_CATEGORY): v.category, (FIELD_BLOCKED): v.blocked }))
                .collect(),
        )
    })
}

const FIELD_SAFETY_RATINGS: &str = "safetyRatings";
const FIELD_PROMPT_FEEDBACK: &str = "promptFeedback";
const FIELD_CATEGORY: &str = "category";
const FIELD_BLOCKED: &str = "blocked";

#[cfg(test)]
#[path = "tests/audit_media_tests.rs"]
mod audit_media_tests;

#[cfg(test)]
#[path = "tests/media_mime_fidelity_tests.rs"]
mod media_mime_fidelity_tests;

#[cfg(test)]
#[path = "tests/audit_stream_cost_tests.rs"]
mod audit_stream_cost_tests;
