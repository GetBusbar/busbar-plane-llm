// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Gemini protocol reader/writer implementation.

use crate::codec::dialect::*;
use crate::codec::framework::ERR_TYPE_OVERLOADED;
use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use crate::codec::usage_count::read_count_u64;
use busbar_contract::http::StatusCode;
use busbar_contract::protocol::*;
use busbar_contract::protocol::{
    ERR_TYPE_AUTHENTICATION, ERR_TYPE_INVALID_REQUEST, ERR_TYPE_NOT_FOUND, ERR_TYPE_PERMISSION,
    ERR_TYPE_RATE_LIMIT,
};
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;
use busbar_contract::upstream::StatusClass;
// Relative path so it resolves both standalone and netted into core.
#[allow(unused_imports)]
// used standalone; redundant with the `busbar_contract::protocol::*` glob when netted into core
use super::proto_codec::*;
// Explicit import so it binds to this crate's own `proto_codec`, not the `busbar_contract::protocol::*` glob.
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
/// header (rung 3, tighter than the shared path suffixes), the `:{action}` path verbs (rung 5), then
/// the `/v1{,beta}/models/` wildcard surface (rung 6). Strengths are ladder positions (lower binds
/// tighter), shared with the sibling dialects' predicates.
fn claims(
    h: busbar_contract::protocol::HeadFields<'_>,
    path: &str,
) -> Option<busbar_contract::protocol::ClaimStrength> {
    use crate::codec::framework::head_field;
    use busbar_contract::protocol::ClaimStrength;
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
/// surface, distinct from the router's `:{verb}` set. A last segment ending in one is Gemini; any
/// other colon-bearing id (an OpenAI fine-tune) falls to the OpenAI residual.
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
fn residual_claims(path: &str) -> Option<busbar_contract::protocol::ClaimStrength> {
    use busbar_contract::protocol::ClaimStrength;
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
    streaming_content_type: Some(busbar_contract::protocol::TEXT_EVENT_STREAM),
    array_stream_shim_key: Some(GEMINI_JSON_ARRAY_SHIM_KEY),
    // No tool id on the wire (Gemini correlates `functionCall`s by name): nothing to reshape.
    native_tool_id_prefix: None,
    ingress_auth: IngressAuth::Bearer,
    // The native credential is the raw key in `x-goog-api-key` (no Bearer), declared as data: the
    // kernel's egress-auth unit presents the lane credential and the key never passes through this plane.
    egress_auth_headers: None,
    egress_auth_lane_constant: false,
    egress_scheme: Some(EgressScheme::header(FIELD_X_GOOG_API_KEY)),
    // The model is in the URL (`/v1beta/models/{model}:generateContent`); the arrival is registered
    // via `busbar_llm::PATH_INGRESS` and the boot parity assert pairs it with `has_model_in_url: true`.
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
    // Google GenAI SDK UA. Re-verify per release; `test_egress_ua_versions_are_pinned_and_present`
    // guards drift. Written on a translated route, on a same-dialect route whose caller sent no
    // user-agent, and on the health probe.
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
    egress_stream_accept: busbar_contract::protocol::TEXT_EVENT_STREAM,
    models_list_envelope: Some(models_list_envelope),
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: Some(vendor_response_metadata),
    // The SDK sends `x-goog-api-key`, which disambiguates the shared list-models surface as Gemini
    // (the `/v1beta` path is handled by the detection fold).
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
/// only when requested. Reported so the seam logs the drop; no other protocol can carry them.
fn vendor_response_metadata(body: &serde_json::Value) -> Vec<&'static str> {
    // Named by wire path, as the drop path reports it.
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
    // The model and the stream ask are the route's (they ride the URL).
    crate::codec::drops::Parked {
        key: keys::MODEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: keys::STREAM,
        holds: crate::codec::drops::Holds::Nothing,
    },
    // `labels` crosses as the caller metadata.
    crate::codec::drops::Parked {
        key: FIELD_LABELS,
        holds: crate::codec::drops::Holds::Nothing,
    },
    // Members of `generationConfig` the reader's code carries; the map file maps the rest.
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
// The per-modality output split is read into the usage detail. A stream frame is an answer.
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
/// SSE). Shared by the route injection (`ingress`), the forward strip
/// (`proxy::strip_router_shim_keys`) and the reader's `modeled_keys` exclusion so it never reaches
/// a backend. The `__busbar` prefix rules out a collision with a real field.
pub const GEMINI_JSON_ARRAY_SHIM_KEY: &str = "__busbar_gemini_json_array";

/// The canonical Gemini bad-API-key message (`google.rpc.Status.message` of a real 400/INVALID_ARGUMENT).
/// Returned by `GeminiWriter::auth_failure_message`; `write_error` matches it to gate the
/// `details[].reason == "API_KEY_INVALID"` ErrorInfo onto exactly that 400.
pub const GEMINI_BAD_KEY_MESSAGE: &str = "API key not valid. Please pass a valid API key.";

/// Cap on distinct tool-call block indices in `state.open_tools` for one SSE stream. The set drains
/// on either terminal path (a `finishReason` chunk, or a prompt-block envelope
/// `candidates_absent` + `promptFeedback.blockReason`), so an upstream streaming unbounded
/// `functionCall` parts without reaching one would grow it until OOM. Past the cap, new tool frames
/// are not recorded and their BlockStart/BlockDelta are not emitted. No real response approaches
/// it. Mirrors the Cohere reader's `MAX_TRACKED_TOOL_FRAMES`.
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
/// not billed (see [`crate::codec::ir::types::IrUsageDetail::traffic_type`]). Not declared by the
/// pinned generativelanguage discovery document (Vertex-only), so it is carried in the IR rather
/// than dropped.
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
/// The table is also the guard: [`gemini_usage_identity_note`] fires when Google's stated total
/// cannot be reached from it, i.e. when Google reports a counter this dialect does not model. The
/// gap is ledgered as the open class `unitemized_tokens` and the WARN names it; a recurring gap is
/// fixed by a new itemized term backed by the wire lock.
const GEMINI_USAGE_ADDITIVE_TERMS: &[&str] = &[
    FIELD_PROMPT_TOKEN_COUNT,
    FIELD_CANDIDATES_TOKEN_COUNT,
    FIELD_THOUGHTS_TOKEN_COUNT,
    FIELD_TOOL_USE_PROMPT_TOKEN_COUNT,
];

/// Stable identifier for the identity [`gemini_usage_identity_note`] checks, carried on
/// [`crate::codec::ir::UsageIdentityNote::identity`] so callers branch on a constant.
const GEMINI_USAGE_IDENTITY: &str = "gemini.usageMetadata";

/// The sum of [`GEMINI_USAGE_ADDITIVE_TERMS`] on one `usageMetadata` object (absent terms are zero).
/// `None` only for an unreadable term, which the usage table has already refused.
fn additive_sum(u: &serde_json::Value) -> Option<u64> {
    GEMINI_USAGE_ADDITIVE_TERMS
        .iter()
        .try_fold(0u64, |sum, k| {
            crate::codec::usage_count::billed_count(u, k).map(|n| sum.saturating_add(n))
        })
        .ok()
}

/// Cross-check what busbar will bill against the total Google stated, and report a disagreement
/// instead of hiding one.
///
/// `summed_total` sums [`GEMINI_USAGE_ADDITIVE_TERMS`] (what the usage table folds in since 1.6.0),
/// so this is a data-driven discrepancy metric over `totalTokenCount`: it fires only when Google's
/// total cannot be reached from the table (an unmodelled counter, or a term whose meaning changed),
/// and the answer is a new recording and table entry, not an adjustment here. `wire_sum` /
/// `unmodelled_term` in the log line say which: the table failing to close is the "new counter"
/// signal; a gap only against `billed` means normalization (cache subtraction) moved the money.
///
/// `None` ordinarily: no `totalTokenCount` (early SSE frames carry an empty `usageMetadata`), or
/// the billed figure matches the total.
///
/// Nothing is zeroed, clamped or back-filled. A total above the bucket sum is the open class
/// `unitemized_tokens` (`IrUsage::to_token_usage`); this note and its WARN say where it came from.
fn gemini_usage_identity_note(
    u: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::UsageIdentityNote> {
    let u = u?;
    // Absent is not zero: a `usageMetadata` with no total has nothing to check, and treating it as 0
    // would flag every ordinary streaming frame.
    let reported_total = u.get(FIELD_TOTAL_TOKEN_COUNT).and_then(read_count_u64)?;
    // What busbar counts: every additive term, which is what the usage table folds into
    // `input_tokens + cache_read_input_tokens + output_tokens`.
    let summed_total = additive_sum(u)?;
    if summed_total == reported_total {
        return None;
    }
    let unaccounted = i64::try_from(reported_total).unwrap_or(i64::MAX)
        - i64::try_from(summed_total).unwrap_or(i64::MAX);
    // Which kind of shortfall? Sum the terms known to be additive from the const table. If that does
    // not close against Google's total, Google reports a counter this dialect does not model and the
    // table needs a new entry and recording. If it closes while `billed` disagrees, the gap is in
    // normalization (e.g. more cached than prompt tokens), not on the wire. The responses differ, so
    // the log line should say which. Read through `billed_count`, as the caller already refused
    // unreadable terms.
    let wire_sum = summed_total;
    let unmodelled_term = wire_sum != reported_total;
    tracing::warn!(
        identity = GEMINI_USAGE_IDENTITY,
        reported_total,
        summed_total,
        unaccounted,
        wire_sum,
        unmodelled_term,
        "gemini usageMetadata does not reconcile: the stated totalTokenCount disagrees with the \
         sum of the counters it names. unmodelled_term=true means Google reports a counter this \
         dialect does not model and GEMINI_USAGE_ADDITIVE_TERMS needs a new entry backed by a \
         recording; false means the wire's own terms close and the gap is in normalization. The \
         itemized counts are ledgered as reported; a positive gap is ledgered as the open class \
         unitemized_tokens."
    );
    Some(crate::codec::ir::UsageIdentityNote {
        reported_total,
        summed_total,
        unaccounted,
        identity: GEMINI_USAGE_IDENTITY,
    })
}

/// JSON key for the opaque response identifier emitted at the top level.
const FIELD_RESPONSE_ID: &str = "responseId";
/// JSON key for the serving model name emitted at the top level.
const FIELD_MODEL_VERSION: &str = "modelVersion";
/// Key for the RFC3339 response-creation timestamp Vertex stamps on every `GenerateContentResponse`.
/// Undeclared by the pinned generativelanguage discovery document (Vertex-only, like
/// [`FIELD_TRAFFIC_TYPE`]); carried in the IR. See [`crate::codec::ir::types::IrResponse::create_time`].
const FIELD_CREATE_TIME: &str = "createTime";

/// Read Gemini/Vertex's top-level `createTime` verbatim (a foreign timestamp parser could reject a
/// Vertex precision/zone quirk). `None` for every non-Vertex Gemini response and non-Gemini protocol.
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
/// the error writer maps status to name and back from this one table.
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
/// No `-`/`_`, so no separator leaks the synthetic boundary.
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
/// The whole token is filled from the host entropy source with no counter overlay (a counter leaves
/// predictable high digits at its position); 16 base62 chars is ~95 bits, collision-free per
/// process. Never panics: on entropy failure the buffer stays the base62 zero char.
///
/// Bytes reduce to base62 by rejection sampling, not `byte % 62` (`256 % 62 != 0` biases residues
/// `0..=7`): any byte `>= RESPONSE_ID_REJECT_THRESHOLD` is skipped and more are drawn.
fn synth_response_id(host: &dyn busbar_contract::codec::CodecHost) -> String {
    let mut token = [b'0'; RESPONSE_ID_TOKEN_LEN];
    let mut filled = 0usize;
    // Bound the refill rounds so a stuck entropy source cannot spin the request path; ~4/256 of
    // bytes are rejected, so a few rounds suffice and the `'0'`-prefilled buffer is the fallback.
    let mut rounds = 0u32;
    const MAX_ROUNDS: u32 = 8;
    while filled < RESPONSE_ID_TOKEN_LEN && rounds < MAX_ROUNDS {
        rounds += 1;
        // A generous batch so one draw usually fills the token despite the ~1.6% reject rate.
        let mut batch = [0u8; RESPONSE_ID_TOKEN_LEN * 2];
        if !host.fill_entropy(&mut batch) {
            break;
        }
        for &byte in batch.iter() {
            if filled >= RESPONSE_ID_TOKEN_LEN {
                break;
            }
            if byte >= RESPONSE_ID_REJECT_THRESHOLD {
                // Biased residue region: reject and resample.
                continue;
            }
            token[filled] = RESPONSE_ID_ALPHABET[(byte % 62) as usize];
            filled += 1;
        }
    }

    // `token` is ASCII base62, so always valid UTF-8; the fallback keeps the path panic-free.
    String::from_utf8(token.to_vec()).unwrap_or_else(|_| "0".repeat(RESPONSE_ID_TOKEN_LEN))
}

/// Synthesize a stable, non-empty tool-call id for a Gemini `functionCall`. The wire carries none,
/// and an empty id on Anthropic/OpenAI egress (which require one to correlate the later
/// `tool_result`) made same-named calls indistinguishable and broke result routing.
///
/// The id is a deterministic hash of `(call_index, function_name, turn_salt)` via stdlib
/// `DefaultHasher` (fixed seed, unlike `HashMap`'s `RandomState`), so it only needs to be stable
/// within a request/response so the synthesized `tool_result` (keyed by function name, Gemini's only
/// handle) and the `tool_use` agree; the index separates repeated names in a turn. The `call_`
/// prefix marks it synthetic. An empty `name` still gives a non-empty id.
///
/// `turn_salt` separates turns: `call_index` restarts at 0 per `read_response`/`read_response_events`
/// call (each response is one turn decoded in isolation), so two turns in one conversation whose
/// first call shares a name (`get_weather` for another city) got the identical id, which
/// Anthropic/OpenAI require to be unique. Response call sites pass the response's `responseId`
/// (present on nearly every real response; see `write_response`'s synth-when-absent handling). The
/// request reader passes `""`: its `call_index` is global across the whole `contents` array.
fn synth_tool_call_id(call_index: usize, function_name: &str, turn_salt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_index.hash(&mut hasher);
    function_name.hash(&mut hasher);
    turn_salt.hash(&mut hasher);
    format!("call_{:016x}", hasher.finish())
}

/// The native call id on a `functionCall` / `functionResponse` object (IR audit GEM-08). Both `id`s
/// are optional; when populated, the response is paired with its call by it. `None` when absent or
/// empty, so the caller falls back to its synthesized id.
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
#[derive(Default)]
struct GeminiCallLedger {
    /// `(id, name, turn, answered)` for every call seen so far, in history order.
    calls: Vec<(String, String, usize, bool)>,
}

impl GeminiCallLedger {
    fn record_call(&mut self, id: &str, name: &str, turn: usize) {
        self.calls
            .push((id.to_string(), name.to_string(), turn, false));
    }

    fn pair_response(&mut self, explicit_id: Option<&str>, name: &str) -> String {
        if let Some(id) = explicit_id {
            if let Some(c) = self.calls.iter_mut().find(|c| !c.3 && c.0 == id) {
                c.3 = true;
            }
            return id.to_string();
        }
        let latest_turn = self
            .calls
            .iter()
            .filter(|c| !c.3 && c.1 == name)
            .map(|c| c.2)
            .max();
        if let Some(turn) = latest_turn {
            if let Some(c) = self
                .calls
                .iter_mut()
                .find(|c| !c.3 && c.1 == name && c.2 == turn)
            {
                c.3 = true;
                return c.0.clone();
            }
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
/// into its IR block. Shared by the request reader, the `functionResponse.parts` reader (GEM-07) and
/// the response reader (generated image/audio, GEM-17). `None` when the part is neither.
///
/// `inlineData` carries any mime type, so images route to `Image` and everything else to the typed
/// `Media` block, whose writers know which target has a native slot (an `audio/*` payload sent to
/// Anthropic as an `image` is a 400). `fileData` has an optional `mimeType`, the only thing saying
/// whether it is an image, PDF or video; absent, an unqualified URL reads as an image.
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
        return Some(
            if mime.is_empty() || mime.to_ascii_lowercase().starts_with(keys::IMAGE_) {
                crate::codec::ir::IrBlock::Image {
                    source: crate::codec::ir::IrImageSource::Url(uri),
                    cache_control: None,
                    detail: None,
                }
            } else {
                crate::codec::ir::IrBlock::Media {
                    kind: crate::codec::ir::IrMediaKind::from_media_type(mime),
                    source: crate::codec::ir::IrImageSource::Url(uri),
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
/// GEM-06). `None` for a non-attachment block or a vendor handle Gemini cannot resolve.
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
/// `fileData` requires a `mimeType`, so a well-formed generic beats omitting it: `application/pdf`
/// (Gemini's own documented example) and container-agnostic audio/video types. A write-side default,
/// never a claim about the bytes; a `Base64` source knows its real mime and never comes here.
fn gemini_mime_for_kind(kind: crate::codec::ir::IrMediaKind) -> &'static str {
    match kind {
        crate::codec::ir::IrMediaKind::Document => "application/pdf",
        crate::codec::ir::IrMediaKind::Audio => keys::AUDIO_MPEG,
        crate::codec::ir::IrMediaKind::Video => "video/mp4",
    }
}

/// A representative `image/*` `mimeType` for a bare-URL image (`fileData` requires one, see above).
/// Derived from the URL's file extension, defaulting to `image/jpeg` when absent or unrecognized. A
/// write-side default, never a claim about the bytes; a `Base64` image never comes here.
fn gemini_image_mime_for_url(uri: &str) -> &'static str {
    // Only the path's extension, lowercased, ignoring any `?query`/`#fragment`.
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
/// Case-insensitive (REST enum upper, docs lower). `None` for an unrecognized level.
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
/// decimal string): `-1` ("the model decides"), `0` (off) or a positive count. `None` for anything
/// else (wrong-typed, fractional, below -1, beyond int32), which the reader answers with Gemini's
/// error envelope.
fn read_gemini_thinking_budget(v: &serde_json::Value) -> Option<i32> {
    let n = match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => i,
            None => {
                let f = n.as_f64()?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return None;
                }
                // An integral float in int32 range; anything wider fails `try_from` below.
                f as i64
            }
        },
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    i32::try_from(n).ok().filter(|n| *n >= -1)
}

/// Normalize a Gemini OpenAPI-subset `Schema` (`parameters`, `responseSchema`) into JSON Schema for
/// the IR (IR audit GEM-11). Gemini spells types upper case (`OBJECT`) and marks optional-null with
/// `nullable: true`, neither valid JSON Schema, so foreign targets got a malformed schema. Walks
/// only schema positions (`properties` values, `items`, `prefixItems`, `anyOf`/`oneOf`/`allOf`, a
/// schema-valued `additionalProperties`, `$defs`/`definitions` values), so an `enum`/`example`/
/// `default` value holding a `type` key is never rewritten. `parametersJsonSchema` /
/// `responseJsonSchema` are already JSON Schema and skip this.
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
        // `nullable: true` becomes a `"null"` member in `type`; `nullable: false` goes. A `nullable`
        // with no string `type` is left alone.
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
/// into Unix epoch seconds, the IR's `created`. `None` for a malformed value, which leaves
/// `created` for the seam to stamp. Fractional seconds are truncated.
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
    // days_from_civil (Howard Hinnant's public-domain algorithm).
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
/// zipped into neutral IR entries. No byte arrays (`bytes: None`; an OpenAI writer synthesizes them).
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
/// to `None`, `ANY` with no `allowedFunctionNames` to `Required`, `ANY` + `allowedFunctionNames:[X]`
/// to `Tool{name:X}`. Absent config/mode or an unrecognized mode is `None`. Takes the whole
/// `toolConfig` object.
fn read_gemini_tool_choice(
    tool_config: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    let fcc = tool_config?.get(FIELD_FUNCTION_CALLING_CONFIG)?;
    let mode = fcc.get(keys::MODE).and_then(|m| m.as_str())?;
    match mode.to_uppercase().as_str() {
        GEMINI_AUTO => Some(crate::codec::ir::IrToolChoice::Auto),
        keys::NONE_UPPER => Some(crate::codec::ir::IrToolChoice::None),
        GEMINI_ANY => {
            // `allowedFunctionNames` is a list but `Tool` models one tool. One name maps to
            // `Tool{name}`; with N>1, `Tool{first}` would invent a stricter constraint, so the
            // directive is `Required` and the subset rides `allowed_tools` (IR-10,
            // `read_gemini_allowed_tools`).
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
/// SDKs reject. An explicitly present value (even `null`) is kept verbatim.
fn empty_object_if_absent(args: Option<&serde_json::Value>) -> serde_json::Value {
    match args {
        Some(v) => v.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Coerce an `IrBlock::ToolUse.input` into a valid `functionCall.args` (a protobuf Struct, so it
/// must be a JSON object). A cross-protocol reader can hand us an array, scalar, `null` or
/// unparseable string, which the backend 400s. As with `ToolResult.response` below: an object passes
/// through byte-identical (keeping Gemini round-trips lossless), `null` becomes `{}`, and any other
/// value is wrapped as `{"args": <value>}`. A string is parsed first and the result coerced; an
/// unparseable one is wrapped as a scalar.
fn coerce_tool_args(input: &serde_json::Value) -> serde_json::Value {
    // A string is a serialized payload: parse it, else keep the string (a scalar). Non-strings as-is.
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
/// distinguishing a prompt-block / error-only envelope from a candidate-bearing chunk. An empty
/// `candidates: []` counts as missing: a native prompt rejection
/// (`{"candidates":[],"promptFeedback":{"blockReason":"SAFETY"}}`) carries one, and keying on
/// presence let it skip the prompt-block arm in both readers (a bare unterminated stream, and a
/// spurious `ir_parse` error from `candidates.is_empty()`), dropping a content-policy block. An
/// empty array with no block reason still falls through to the handling below.
fn candidates_absent(data: &serde_json::Value) -> bool {
    match data.get(FIELD_CANDIDATES).and_then(|c| c.as_array()) {
        Some(arr) => arr.is_empty(),
        None => true,
    }
}

/// A top-level `promptFeedback.blockReason` (the prompt-level block signal), e.g.
/// `{"promptFeedback":{"blockReason":"SAFETY"}}`. The raw reason (SAFETY / BLOCKLIST /
/// PROHIBITED_CONTENT / OTHER / …) for mapping to a stop reason; `None` when absent or empty.
fn prompt_block_reason(data: &serde_json::Value) -> Option<&str> {
    data.get(FIELD_PROMPT_FEEDBACK)
        .and_then(|pf| pf.get("blockReason"))
        .and_then(|r| r.as_str())
        .filter(|s| !s.is_empty())
}

/// Map a candidate `finishReason` to a canonical IR stop reason. `STOP`/`MAX_TOKENS`/`SAFETY` map
/// to `end_turn`/`max_tokens`/`safety`. Content-policy stops (`RECITATION`, `IMAGE_SAFETY`, `SPII`,
/// `BLOCKLIST`, `PROHIBITED_CONTENT`) map to `safety`. `MALFORMED_FUNCTION_CALL` (an unparseable tool
/// call: generation failed, there is no call to run) maps to `error`, not `tool_use`, which would
/// tell the client to execute a call that does not exist. `OTHER`, `LANGUAGE` and unknown future
/// reasons map to `Other` (they were once lowercased verbatim into values no SDK enum recognizes).
/// A Gemini round-trip is unaffected: the writer emits `Other` as `OTHER` and `safety` as `SAFETY`.
/// Do not simplify `_ => S::Other` to `S::EndTurn`: that would turn a Gemini `OTHER` stop into `STOP`.
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
        // Image-generation content-policy stops (IR audit GEM-15), as their text siblings above.
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" => S::Safety,
        // An invalid function call is an abnormal stop with no runnable call. `UNEXPECTED_TOOL_CALL`
        // (a tool called while none was enabled) is the same failed generation (GEM-15).
        GEMINI_FINISH_MALFORMED_FUNCTION_CALL | "UNEXPECTED_TOOL_CALL" => S::Error,
        // OTHER / LANGUAGE / any novel future reason.
        _ => S::Other,
    }
}

/// Map a `promptFeedback.blockReason` to a canonical IR stop reason. A prompt block refuses the
/// input on content policy, so the well-known reasons surface as `safety` (as candidate-level
/// `SAFETY` does); any other reason is lowercased so a novel one is surfaced, not dropped.
fn prompt_block_stop_reason(block_reason: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match block_reason {
        // RECITATION maps to Safety at the candidate level (see GEMINI_FINISH_RECITATION); same here.
        GEMINI_FINISH_SAFETY
        | GEMINI_BLOCKLIST
        | GEMINI_FINISH_PROHIBITED_CONTENT
        | GEMINI_FINISH_RECITATION
        // A prompt blocked for its image content is the same policy block (IR audit GEM-15).
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
        // A refusal has no Gemini finishReason: written as `OTHER`. `SAFETY` names a safety filter,
        // which a refusal is not, so writing it would substitute a value the source never stated.
        S::Refusal | S::Error | S::PauseTurn | S::Other => GEMINI_FINISH_OTHER,
    }
}

/// Map a canonical `StatusClass` onto the `(HTTP code, google.rpc.Code name)` pair Gemini uses in
/// its `google.rpc.Status` error envelope. Exhaustive over `StatusClass` (no `_ =>` catch-all) so
/// a new class forces a conscious choice here rather than silently degrading to INTERNAL.
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
/// `error` object, to a `StatusClass`. Read-side inverse of `gemini_stream_error_code_status`, so
/// the ingress writer can end the stream with a protocol-shaped error frame.
///
/// The UPPER_SNAKE google.rpc.Code `status` is preferred (what a native SDK branches on), falling
/// back to the numeric HTTP `code` when `status` is absent or unrecognized. A name we do not model
/// binds to a named arm, not a degrading `_` (`&str` matches are never exhaustive). An absent or
/// unknown code gives `ServerError`: retryable, trips the breaker, never masks a failure as success.
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
                return StatusClass::ServerError
            }
            // A status outside the modeled google.rpc.Code set falls through to the numeric `code`
            // mapping. Named (not `_`) per the no-catch-all rule.
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
        // Any 5xx or absent/unknown code: ServerError, the breaker-tripping default.
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
    /// [`busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT`].
    translate_cap: usize,
}

/// The streamed answer text [`GeminiWriter::stream_text`] accumulates: the concatenated `text` and
/// one `(ir_block_index, byte_start)` per text block in order of first text, with the character
/// count and index marks kept as it grows so a citation converts without walking the text.
#[derive(Clone, Debug, Default)]
struct GeminiStreamText {
    text: String,
    block_starts: Vec<(usize, usize)>,
    chars: usize,
    marks: Vec<usize>,
}

/// Value-namespace constructor for [`GeminiWriter`], so sites writing the bare `GeminiWriter`
/// literal (`Protocol::gemini()`, tests) compile unchanged. Each use inlines a fresh writer with an
/// empty `open_tool` buffer, giving the per-stream scoping the single-frame functionCall fix needs.
///
/// `declare_interior_mutable_const` is suppressed deliberately: a `static` would share one buffer
/// across every stream and bleed one stream's open tool name into another. Mirrors `ResponsesWriter`.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const GeminiWriter: GeminiWriter = GeminiWriter {
    stream: StreamCell::new(GeminiStream {
        open_tools: Vec::new(),
        response_id: None,
        stream_text: GeminiStreamText {
            text: String::new(),
            block_starts: Vec::new(),
            chars: 0,
            marks: Vec::new(),
        },
        translate_cap: busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    }),
};

impl GeminiWriter {
    /// The stream's `responseId`: the first wins. Returns the captured id if any, else captures and
    /// returns `mint()`, so a duplicate identity frame restates the id the client has. A poisoned
    /// lock gives the freshly minted id.
    fn carried_response_id(&self, mint: impl FnOnce() -> String) -> String {
        self.stream
            .lock()
            .response_id
            .get_or_insert_with(mint)
            .clone()
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
/// `blocked` as both `flagged` and `blocked`. The probability is Google's scale and does not cross.
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
/// probability). `None` when there are none.
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
