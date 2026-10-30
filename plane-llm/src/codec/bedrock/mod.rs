// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Bedrock Converse protocol reader/writer implementation.

use crate::codec::ir::IrStreamEvent;
use crate::codec::keys;
use busbar_contract::http::{HeaderName, HeaderValue, StatusCode};
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
use crate::codec::usage_count::{CountRead, CountSlot, UsageCount};
use busbar_contract::ir::egress_prep::LaneCaps;
// Explicit import so it binds to this crate's own `proto_codec`, not the `busbar_contract::protocol::*` glob.
#[allow(unused_imports)]
use super::proto_codec::{Protocol, ProtocolReader, ProtocolWriter, StreamFraming};

mod citations;
pub mod handler;
#[rustfmt::skip]
#[path = "map.gen.rs"]
mod map;
mod reader;
mod writer;

use self::citations::{
    read_bedrock_citation, read_bedrock_citations_content, write_bedrock_citation,
};

// One spelling per wire word: words in `crate::codec::keys` are spelled there; the rest are this
// dialect's own, defined once.

// Converse request/response member names (camelCase, as the wire spells them).
const ADDITIONAL_MODEL_REQUEST_FIELDS: &str = "additionalModelRequestFields";
const ADDITIONAL_MODEL_RESPONSE_FIELDS: &str = "additionalModelResponseFields";
const CACHE_DETAILS: &str = "cacheDetails";
const CACHE_POINT: &str = "cachePoint";
/// The Converse answer's (and the stream `metadata` frame's) served-tier member: `{"type": <tier>}`.
const SERVICE_TIER_CAMEL: &str = "serviceTier";
const CACHE_READ_INPUT_TOKENS: &str = "cacheReadInputTokens";
const CACHE_WRITE_INPUT_TOKENS: &str = "cacheWriteInputTokens";
const CFG_SCALE: &str = "cfgScale";
const CITATIONS_CONTENT: &str = "citationsContent";
const CONTENT_BLOCK_INDEX: &str = "contentBlockIndex";
const DOCUMENT_CHAR: &str = "documentChar";
const DOCUMENT_INDEX: &str = "documentIndex";
const DUNDER_TYPE: &str = "__type";
const GUARD_CONTENT: &str = "guardContent";
const IMAGE_GENERATION_CONFIG: &str = "imageGenerationConfig";
const INFERENCE_CONFIG: &str = "inferenceConfig";
const INPUT_SCHEMA_CAMEL: &str = "inputSchema";
const INPUT_TEXT: &str = "inputText";
const INPUT_TEXT_TOKEN_COUNT: &str = "inputTextTokenCount";
const INPUT_TOKENS_CAMEL: &str = "inputTokens";
const INTERNAL_SERVER_EXCEPTION: &str = "internalServerException";
const JSON_SCHEMA_CAMEL: &str = "jsonSchema";
const MAX_TOKENS_CAMEL: &str = "maxTokens";
const MODEL_STREAM_ERROR_EXCEPTION: &str = "modelStreamErrorException";
const NEGATIVE_TEXT: &str = "negativeText";
const NUMBER_OF_IMAGES: &str = "numberOfImages";
const OUTPUT_CONFIG_CAMEL: &str = "outputConfig";
const OUTPUT_TOKENS_CAMEL: &str = "outputTokens";
const REASONING_CONTENT: &str = "reasoningContent";
const REASONING_TEXT: &str = "reasoningText";
const REDACTED_CONTENT: &str = "redactedContent";
/// A Converse `searchResult` content block (request content and `toolResult` content).
const SEARCH_RESULT_CAMEL: &str = "searchResult";
const SEARCH_RESULT_INDEX: &str = "searchResultIndex";
const SEARCH_RESULT_LOCATION_CAMEL: &str = "searchResultLocation";
const SERVICE_UNAVAILABLE_EXCEPTION: &str = "serviceUnavailableException";
const SOURCE_CONTENT: &str = "sourceContent";
const STOP_REASON: &str = "stopReason";
const TEXT_FORMAT: &str = "textFormat";
const TEXT_TO_IMAGE_PARAMS: &str = "textToImageParams";
const THROTTLING_EXCEPTION: &str = "throttlingException";
const TOOL_CHOICE_CAMEL: &str = "toolChoice";
const TOOL_RESULT: &str = "toolResult";
const TOOL_SPEC: &str = "toolSpec";
const TOOL_USE_CAMEL: &str = "toolUse";
const TOOL_USE_ID: &str = "toolUseId";
const TOP_K_CAMEL: &str = "topK";
const TOTAL_TOKENS_CAMEL: &str = "totalTokens";
const VALIDATION_EXCEPTION: &str = "validationException";

// Media container / format tokens and their MIME spellings.
const APPLICATION_MSWORD: &str = "application/msword";
const APPLICATION_VND_MS_EXCEL: &str = "application/vnd.ms-excel";
const APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_SPREADSHEETML_SHEET: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_WORDPROCESSINGML_DOCUMENT: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const DOC: &str = "doc";
const DOCX: &str = "docx";
const FLV: &str = "flv";
const MKV: &str = "mkv";
const MOV: &str = "mov";
const FORMAT_MP4: &str = "mp4";
const MPEG: &str = "mpeg";
const MPG: &str = "mpg";
const PDF: &str = "pdf";
const THREE_GP: &str = "three_gp";
const WEBM: &str = "webm";
const WMV: &str = "wmv";
const XLS: &str = "xls";
const XLSX: &str = "xlsx";

// Other wire words.
const B: &str = "b";
const CONTENT_FILTERED: &str = "content_filtered";
const HEIGHT: &str = "height";
const IMAGES: &str = "images";
const LOCATION: &str = "location";
const NORMALIZE: &str = "normalize";
const S3_LOCATION: &str = "s3Location";
const STRUCTURE: &str = "structure";
const TOP_K: &str = "top_k";
const TRACE: &str = "trace";
const TTL: &str = "ttl";
const VIDEO: &str = "video";
const WIDTH: &str = "width";

/// Build this dialect's wire codec. A fresh instance per resolution.
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, BedrockReader, BedrockWriter)
}

/// Router detection. The AWS SigV4 `Authorization: AWS4-HMAC-SHA256…` signature is the tightest
/// claim of any dialect (rung 1, path-independent), then `/converse` (rung 12) and
/// `/model/{id}/invoke` (rung 13).
fn claims(
    h: busbar_contract::protocol::HeadFields<'_>,
    path: &str,
) -> Option<busbar_contract::protocol::ClaimStrength> {
    use crate::codec::framework::head_field;
    use busbar_contract::protocol::ClaimStrength;
    if head_field(h, "authorization")
        .is_some_and(|a| a.is_ascii() && a.starts_with(b"AWS4-HMAC-SHA256"))
    {
        return Some(ClaimStrength(1));
    }
    if path.contains("/converse") {
        return Some(ClaimStrength(12));
    }
    if path.starts_with("/model/") && path.ends_with("/invoke") {
        return Some(ClaimStrength(13));
    }
    None
}

/// Headerless residual detection: a `/model/{id}/converse[-stream]` path (rung 30). The
/// `/converse` suffix is load-bearing: a non-Converse `/model/…` path must not wear a Bedrock envelope.
fn residual_claims(path: &str) -> Option<busbar_contract::protocol::ClaimStrength> {
    if path.starts_with("/model/")
        && (path.ends_with("/converse") || path.ends_with("/converse-stream"))
    {
        return Some(busbar_contract::protocol::ClaimStrength(30));
    }
    None
}

/// Response-side untranslatable metadata: the guardrail assessment under a top-level `trace`
/// (`trace.guardrail`), present only when requested. Reported so the seam logs the drop, since no
/// other protocol can carry it.
fn vendor_response_metadata(body: &serde_json::Value) -> Vec<&'static str> {
    [TRACE]
        .into_iter()
        .filter(|k| body.get(k).is_some())
        .collect()
}

/// Bedrock's declaration. The only protocol with SigV4 ingress auth and a non-SSE streaming
/// content type.
pub const DECL: ProtocolDecl = ProtocolDecl {
    name: VENDOR_NAME,
    codec: dialect_codec!(VENDOR_NAME),
    handler: Some(&handler::BedrockRequestHandler),
    verbs: &[
        busbar_contract::operation::OpVerb::CHAT,
        busbar_contract::operation::OpVerb::EMBEDDINGS,
        busbar_contract::operation::OpVerb::IMAGE,
        busbar_contract::operation::OpVerb::RERANK,
    ],
    head_keys: super::proto_codec::LLM_CHAT_HEAD_KEYS,
    // Bedrock ingress expects a BINARY eventstream body, not SSE: mislabeling it breaks the SDK.
    streaming_content_type: Some(APPLICATION_VND_AMAZON_EVENTSTREAM),
    array_stream_shim_key: None,
    // `tooluse_…` is Bedrock's documented native tool-call id shape.
    native_tool_id_prefix: Some("tooluse_"),
    ingress_auth: IngressAuth::SigV4,
    // SigV4 is this dialect's own egress scheme, declared as data: the service it signs for, the
    // region as a function of the endpoint host (`us-east-1` fallback), and the content type the
    // signature covers. The kernel's egress-auth unit holds the lane credential
    // (`ACCESS:SECRET[:SESSION]`) and signs under a teller-minted grant, so the secret never passes
    // through this plane. Never lane-constant: every signature covers body, time and path.
    egress_auth_headers: None,
    egress_auth_lane_constant: false,
    egress_scheme: Some(EgressScheme::SigV4 {
        service: VENDOR_NAME,
        region_of_host: declared_sigv4_region,
        default_region: "us-east-1",
        content_type: busbar_contract::protocol::APPLICATION_JSON,
    }),
    // The model is in the URL (`/model/{model_id}/converse`, `/converse-stream`, `/invoke`); the
    // arrival is registered via `busbar_llm::PATH_INGRESS` and the boot parity assert pairs it with
    // `has_model_in_url: true` below.
    stream_usage_requires_opt_in: false,
    requires_max_tokens: false,
    stop_sequence_cap: None,
    cache_markers_model_gated: true,
    fills_thought_signature: false,
    frame_after_message_start: None,
    reshapes_body_at_path_base: false,
    max_cache_control_breakpoints: None,
    // 400 with `ServiceQuotaExceededException`: a spent budget is not transient, and AWS SDKs retry
    // a 429 `ThrottlingException` with backoff.
    quota_exceeded_status: busbar_contract::http::StatusCode::BAD_REQUEST.as_u16(),
    ingress_is_eventstream: true,
    emits_sse_done_terminator: false,
    max_citations_per_delta: Some(1),
    // AWS Bedrock is reached via boto3/botocore. Re-verify per release;
    // `test_egress_ua_versions_are_pinned_and_present` guards drift. Written on a translated route,
    // on a same-dialect route whose caller sent no user-agent, and on the health probe.
    egress_user_agent: "Boto3/1.35.0 md/Botocore#1.35.0",
    has_model_in_url: true,
    auth_failure_status_and_kind: (
        busbar_contract::http::StatusCode::FORBIDDEN.as_u16(),
        keys::AUTH_WORD,
    ),
    ingress_relays_amzn_headers: true,
    ingress_relayed_response_header_names: &[HDR_AMZN_REQUEST_ID, HDR_AMZN_ERROR_TYPE],
    auth_failure_message: "",
    uses_array_stream_shim: false,
    has_native_path_not_found: false,
    // botocore sends the binary eventstream `Accept` on `ConverseStream`; non-stream is `application/json`.
    egress_stream_accept: APPLICATION_VND_AMAZON_EVENTSTREAM,
    // No list-models surface: discovery is not a `/v1/models` GET.
    models_list_envelope: None,
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: Some(vendor_response_metadata),
    // No fingerprint header on the shared list-models surface (the `AWS4-HMAC-SHA256` credential
    // must not steer a models-list GET; the OpenAI residual wins).
    list_models_fingerprint_headers: &[],
    static_headers: &[],
};

/// This dialect's registration (its one line is in `crate::codec::DIALECTS`).
pub(crate) const ENTRY: super::proto_codec::DialectEntry = super::proto_codec::DialectEntry {
    decl: &DECL,
    protocol,
    with_writer: |f| {
        let w = BedrockWriter;
        f(&w)
    },
    with_reader: |f| f(&BedrockReader),
    leaf: &handler::LEAF,
};

/// The two response headers a native Bedrock endpoint always emits (lowercase): the request id the
/// SDK surfaces via `*Output::request_id()`, and the error-type header it reads before the body
/// `__type`. Surfaced externally via the writer vtable (`BedrockWriter::ingress_response_request_id` /
/// `ingress_relayed_response_header_names`).
const HDR_AMZN_REQUEST_ID: &str = "x-amzn-requestid";
const HDR_AMZN_ERROR_TYPE: &str = "x-amzn-errortype";

/// Exception names used by both `error_kind_to_bedrock_type` and `bedrock_stream_exception_for`,
/// named once so they cannot drift.
const EXC_THROTTLING: &str = "ThrottlingException";
const EXC_VALIDATION: &str = "ValidationException";
const EXC_SERVICE_UNAVAILABLE: &str = "ServiceUnavailableException";
const EXC_INTERNAL_SERVER: &str = "InternalServerException";

/// The binary framing content-type of Converse streaming, for both the response `Content-Type` and
/// the egress `Accept` of a `ConverseStream` call.
const APPLICATION_VND_AMAZON_EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

/// AWS's spelling of the "overloaded" error type in `__type` (`ServiceUnavailableException` maps
/// back to it). Distinct from busbar's own `busbar_contract::protocol::KIND_OVERLOADED`; both map to
/// `ServiceUnavailableException` via `error_kind_to_bedrock_type`.
const ERR_TYPE_OVERLOADED: &str = crate::codec::framework::ERR_TYPE_OVERLOADED;

/// The dialect's declared refusals (`refusals.json`): each row names the busbar refusal kinds it
/// answers, the exception and its HTTP status. The runtime answers a listed kind with the row's
/// exception ([`error_kind_to_bedrock_type`]) and the conformance rig reads the same table.
pub const REFUSALS_JSON: &str = include_str!("refusals.json");

/// The exception [`REFUSALS_JSON`] declares for `kind`, or `None` when no row lists it.
fn declared_refusal(kind: &str) -> Option<&'static str> {
    type Rows = Vec<(Vec<String>, &'static str)>;
    static ROWS: std::sync::OnceLock<Rows> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| {
        let doc: serde_json::Value = serde_json::from_str(REFUSALS_JSON).unwrap_or_default();
        doc["refusals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let exception: &'static str =
                    Box::leak(row["exception"].as_str()?.to_owned().into_boxed_str());
                let kinds = row["kinds"]
                    .as_array()?
                    .iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .collect();
                Some((kinds, exception))
            })
            .collect()
    })
    .iter()
    .find(|(kinds, _)| kinds.iter().any(|k| k == kind))
    .map(|(_, exception)| *exception)
}

/// Map busbar's generic error `kind` to the Bedrock Converse exception name in `__type`. The
/// Converse error model is a closed set (`AccessDeniedException`, `InternalServerException`,
/// `ModelErrorException`, `ModelNotReadyException`, `ModelTimeoutException`,
/// `ResourceNotFoundException`, `ServiceUnavailableException`, `ThrottlingException`,
/// `ValidationException`) plus `ServiceQuotaExceededException`, which an exhausted quota or budget
/// answers: it is not retryable, where AWS SDKs retry `ThrottlingException` with backoff. A kind
/// with no counterpart falls back to `ValidationException` so `__type` is always a real AWS name.
/// Inverse of the `__type` token `extract_error` reads, so a same-protocol error round-trips.
pub fn error_kind_to_bedrock_type(kind: &str) -> &'static str {
    if let Some(exception) = declared_refusal(kind) {
        return exception;
    }
    match kind {
        ERR_TYPE_INVALID_REQUEST | "invalid_request" | "validation" | "bad_request" => {
            EXC_VALIDATION
        }
        ERR_TYPE_RATE_LIMIT | "rate_limit" | "too_many_requests" | "throttling" => EXC_THROTTLING,
        ERR_TYPE_AUTHENTICATION
        | ERR_TYPE_PERMISSION
        | keys::AUTH_WORD
        | "forbidden"
        | "unauthorized" => "AccessDeniedException",
        "not_found" | ERR_TYPE_NOT_FOUND | "model_not_found" => "ResourceNotFoundException",
        crate::codec::framework::KIND_TIMEOUT | "model_timeout" => "ModelTimeoutException",
        busbar_contract::protocol::KIND_OVERLOADED
        | ERR_TYPE_OVERLOADED
        | "service_unavailable"
        | "unavailable" => EXC_SERVICE_UNAVAILABLE,
        busbar_contract::protocol::KIND_API_ERROR
        | "internal_error"
        | crate::codec::framework::KIND_SERVER_ERROR => EXC_INTERNAL_SERVER,
        // No native counterpart: the generic client-error exception, still a real AWS name.
        _ => EXC_VALIDATION,
    }
}

/// Mint a UUID-v4-shaped request id for the `x-amzn-RequestId` header, which a native response
/// always carries (success and error, stream and non-stream; its absence is a proxy tell). Uses the
/// OS CSPRNG and returns `None` (header omitted) if entropy is unavailable; this is on the request
/// path and must never panic. Every path reaches this through the writer vtable.
pub fn synth_amzn_request_id(host: &dyn busbar_contract::codec::CodecHost) -> Option<String> {
    let mut buf = [0u8; 16];
    if !host.fill_entropy(&mut buf) {
        return None;
    }
    // RFC 4122 v4 layout (version and variant bits).
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    // One allocation for the 32-char lowercase hex string.
    let s = ::hex::encode(buf);
    Some(format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    ))
}

/// Attach `x-amzn-RequestId` and `x-amzn-errortype` (mirroring the body `__type` via
/// `error_kind_to_bedrock_type`) to an already-built error response, as a native one always
/// carries. Dispatched only through `BedrockWriter::attach_error_response_headers`, so callers
/// cannot drift on which headers a Bedrock error carries. Best-effort: a failed header is skipped,
/// never a panic on the request path.
fn attach_bedrock_error_headers(
    host: &dyn busbar_contract::codec::CodecHost,
    headers: &mut busbar_contract::http::HeaderMap,
    kind: &str,
) {
    if let Some(id) = synth_amzn_request_id(host) {
        if let Ok(hv) = HeaderValue::from_str(&id) {
            headers.insert(HeaderName::from_static(HDR_AMZN_REQUEST_ID), hv);
        }
    }
    let errortype = error_kind_to_bedrock_type(kind);
    if let Ok(hv) = HeaderValue::from_str(errortype) {
        headers.insert(HeaderName::from_static(HDR_AMZN_ERROR_TYPE), hv);
    }
}

/// Map a mid-stream `IrError` to the ConverseStream output-union member name the SDK decoder
/// recognizes, plus the message.
///
/// Distinct from `error_kind_to_bedrock_type`: the modeled mid-stream errors are a smaller union
/// (`InternalServerException`, `ModelStreamErrorException`, `ValidationException`,
/// `ThrottlingException`, `ServiceUnavailableException`). A request-level name such as
/// `ModelTimeoutException` would be an unmodeled event the SDK cannot raise as typed, so every
/// class folds onto one of the five:
///
/// - `RateLimit` to `ThrottlingException`
/// - `Overloaded` to `ServiceUnavailableException`
/// - `ClientError` / `ContextLength` to `ValidationException`
/// - `Timeout` to `ModelStreamErrorException`
/// - `Auth` / `Billing` / `ServerError` / `Network` to `InternalServerException`
///
/// Each class is matched explicitly (no catch-all) so a new `StatusClass` fails to compile here.
/// Shared by `write_response_exception` and the `write_response_event` Error arm. The message
/// prefers the upstream's `provider_signal`, else the exception name.
fn bedrock_stream_exception_for(
    err: &busbar_contract::protocol::IrError,
) -> (&'static str, String) {
    let exception_name = match err.class {
        StatusClass::RateLimit => EXC_THROTTLING,
        StatusClass::Overloaded => EXC_SERVICE_UNAVAILABLE,
        StatusClass::ClientError | StatusClass::ContextLength => EXC_VALIDATION,
        StatusClass::Timeout => "ModelStreamErrorException",
        StatusClass::Auth
        | StatusClass::Billing
        | StatusClass::ServerError
        | StatusClass::Network => EXC_INTERNAL_SERVER,
    };
    let message = err
        .provider_signal
        .clone()
        .unwrap_or_else(|| exception_name.to_string());
    (exception_name, message)
}

/// `extra` key under which the reader stashes native Converse `cachePoint` blocks
/// (`{"cachePoint": {"type": "default"}}`) found inside `system` and each message's `content`.
///
/// `IrBlock` has no counterpart, so without the stash a same-protocol passthrough would drop them
/// and disable the caller's prompt caching. It is a Bedrock-native marker, so `extra` is right: it
/// survives a same-protocol round-trip and is cleared on the cross-protocol seam.
///
/// Each block's original absolute index in its native array is recorded so `write_request` splices
/// it back in place. Shape:
/// ```json
/// {
///   "system":   [ { "i": <usize>, "block": <cachePoint value> }, ... ],
///   "messages": [ { "m": <usize>, "i": <usize>, "block": <cachePoint value> }, ... ]
/// }
/// ```
/// The `__busbar` prefix avoids colliding with a real top-level key, and `write_request` consumes
/// it, so the sentinel never reaches the wire.
const CACHE_POINTS_SENTINEL: &str = "__busbar_bedrock_cache_points";

/// `extra` key under which the reader stashes native Converse `guardContent` blocks (inline
/// Guardrails markers, `{"guardContent": {"text": {"text", "qualifiers"}}}` or `{"image": ...}`)
/// found inside `system` and each message's `content`.
///
/// `IrBlock` cannot express the qualifiers (`grounding_source` / `query` / `guard_content`) that
/// tell Guardrails which spans to evaluate, so without the stash a same-protocol passthrough would
/// drop the caller's inline classification. Bedrock-native, so it lives in `extra` like the
/// `cachePoint` stash, with the same `{ "i", "block" }` / `{ "m", "i", "block" }` shape, and
/// `write_request` splices it back via `splice_cache_points`. The sentinel never reaches the wire.
const GUARD_CONTENT_SENTINEL: &str = "__busbar_bedrock_guard_content";

/// `extra` key under which the reader stashes native Converse top-level `document` and `video`
/// blocks in each message's `content`.
///
/// `IrBlock` cannot express their format plus source (`bytes`/`s3Location`), so without the stash a
/// same-protocol passthrough would drop an uploaded PDF. Bedrock-native, so it lives in `extra` like
/// the `guardContent` stash (`{ "m", "i", "block" }`), spliced back by `write_request` via
/// `splice_cache_points`. The sentinel never reaches the wire.
const DOC_VIDEO_SENTINEL: &str = "__busbar_bedrock_doc_video";

/// ConverseStream event-type names, the discriminator on every stream frame (the `:event-type`
/// header, surfaced as the `"type"` tag on the decoded JSON). The set is closed; exception frames
/// carry their own type names.
const ET_MESSAGE_START: &str = "messageStart";
const ET_CONTENT_BLOCK_START: &str = "contentBlockStart";
const ET_CONTENT_BLOCK_DELTA: &str = "contentBlockDelta";
const ET_CONTENT_BLOCK_STOP: &str = "contentBlockStop";
const ET_MESSAGE_STOP: &str = "messageStop";
const ET_METADATA: &str = "metadata";

/// The `metadata`-frame `metrics` object and its `latencyMs`, the stream's end-to-end latency
/// (injected by `BedrockStreamFraming::inject_streaming_metrics`).
const FIELD_METRICS: &str = "metrics";
const FIELD_LATENCY_MS: &str = "latencyMs";

/// Source-spelling hint for `top_k`. Bedrock carries it in `additionalModelRequestFields` as
/// `top_k` or `topK` (some model families need the camelCase form). The reader lifts either into
/// the IR `top_k`; to avoid renaming a Bedrock passthrough that arrived as `topK`, it stamps this
/// sentinel in `extra`. The writer re-emits `topK` (else `top_k`) and consumes the sentinel.
/// `extra` is cleared on the cross-protocol seam, so cross-protocol egress emits `top_k`.
const TOP_K_CAMEL_SENTINEL: &str = "__busbar_top_k_camel";

/// The Converse request content grammar (`codec::drops`): a union keyed by its kind member. A block
/// of any other kind (an inline `guardContent` marker) is named as dropped on a translate attempt.
const REQUEST_BLOCKS: &[crate::codec::drops::Blocks] = &[
    crate::codec::drops::Blocks {
        at: &["messages[]", "content[]"],
        tag: None,
        modelled: &[
            keys::TEXT,
            keys::IMAGE,
            keys::DOCUMENT,
            VIDEO,
            TOOL_USE_CAMEL,
            TOOL_RESULT,
            REASONING_CONTENT,
            CITATIONS_CONTENT,
            CACHE_POINT,
        ],
        companions: &[],
    },
    crate::codec::drops::Blocks {
        at: &["system[]"],
        tag: None,
        modelled: &[keys::TEXT, CACHE_POINT],
        companions: &[],
    },
];

/// How this dialect spells each IR content-block kind (a dropped block's warn names it so).
const IR_BLOCK_KINDS: &[(&str, &str)] = &[
    (crate::codec::drops::kind::TEXT, keys::TEXT),
    (crate::codec::drops::kind::IMAGE, keys::IMAGE),
    (crate::codec::drops::kind::DOCUMENT, keys::DOCUMENT),
    (crate::codec::drops::kind::VIDEO, VIDEO),
    (crate::codec::drops::kind::THINKING, REASONING_CONTENT),
    (crate::codec::drops::kind::TOOL_USE, TOOL_USE_CAMEL),
    (crate::codec::drops::kind::TOOL_RESULT, TOOL_RESULT),
];

/// IR request members the reader carries by code from a path no map-file row names.
const REQUEST_CODE_NAMES: &[(&str, &str)] = &[
    (
        crate::codec::drops::name::RESPONSE_FORMAT,
        "outputConfig.textFormat",
    ),
    (crate::codec::drops::name::METADATA, FIELD_REQUEST_METADATA),
    // Converse has no `top_k`: the reader takes it from the model-specific fields.
    (
        crate::codec::drops::name::TOP_K,
        "additionalModelRequestFields.top_k",
    ),
];

const RESPONSE_BLOCKS: &[crate::codec::drops::Blocks] = &[crate::codec::drops::Blocks {
    at: &["output", "message", "content[]"],
    tag: None,
    modelled: &[
        keys::TEXT,
        TOOL_USE_CAMEL,
        CITATIONS_CONTENT,
        REASONING_CONTENT,
        keys::IMAGE,
    ],
    companions: &[],
}];

/// What the Converse reader parks in `extra` beside the members its map file does not model.
const PARKED: &[crate::codec::drops::Parked] = &[
    // Positional stashes for a same-dialect write: cache points and documents cross as IR cache
    // breakpoints and media; `REQUEST_BLOCKS` names the guard markers; `topK` spelling is a hint.
    crate::codec::drops::Parked {
        key: CACHE_POINTS_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: GUARD_CONTENT_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: DOC_VIDEO_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    crate::codec::drops::Parked {
        key: TOP_K_CAMEL_SENTINEL,
        holds: crate::codec::drops::Holds::Nothing,
    },
    // `requestMetadata` crosses as the caller metadata.
    crate::codec::drops::Parked {
        key: FIELD_REQUEST_METADATA,
        holds: crate::codec::drops::Holds::Nothing,
    },
    // Members of each config object the reader's code carries; the map file maps the rest.
    crate::codec::drops::Parked {
        key: INFERENCE_CONFIG,
        holds: crate::codec::drops::Holds::Members(&[MAX_TOKENS_CAMEL]),
    },
    crate::codec::drops::Parked {
        key: ADDITIONAL_MODEL_REQUEST_FIELDS,
        // The reasoning asks that ride here (`thinking`, `output_config`, Nova `reasoningConfig`)
        // do not cross: the drop walk names them.
        holds: crate::codec::drops::Holds::Members(&[TOP_K, TOP_K_CAMEL]),
    },
    crate::codec::drops::Parked {
        key: keys::TOOL_CONFIG,
        holds: crate::codec::drops::Holds::Members(&[keys::TOOLS, TOOL_CHOICE_CAMEL]),
    },
    crate::codec::drops::Parked {
        key: OUTPUT_CONFIG_CAMEL,
        holds: crate::codec::drops::Holds::Members(&[TEXT_FORMAT]),
    },
    crate::codec::drops::Parked {
        key: SERVICE_TIER_CAMEL,
        holds: crate::codec::drops::Holds::Members(&[]),
    },
];

/// What this dialect's answers carry beyond its map file's rows (drop walk, design F3 "Drops").
// The per-TTL cache split is read into the usage detail; a stream exception is the terminal error.
const RESPONSE_CODE: &[&str] = &["usage.cacheDetails"];
const STREAM_CODE: &[&str] = &[
    INTERNAL_SERVER_EXCEPTION,
    MODEL_STREAM_ERROR_EXCEPTION,
    SERVICE_UNAVAILABLE_EXCEPTION,
    THROTTLING_EXCEPTION,
    VALIDATION_EXCEPTION,
];

/// Answer paths inside a carried subtree that its code does not carry (where a coarse map row covers them).
const RESPONSE_DROPS: &[&str] = &[
    "output.message.content[].searchResult",
    "output.message.content[].toolAddition",
    "output.message.content[].toolRemoval",
    "output.message.content[].toolResult.content[].searchResult",
    "performanceConfig.latency",
    TRACE,
];
const STREAM_DROPS: &[&str] = &[
    "metadata.performanceConfig.latency",
    "metadata.serviceTier",
    "metadata.trace.guardrail",
    "metadata.trace.promptRouter",
    "modelStreamErrorException.originalMessage",
    "modelStreamErrorException.originalStatusCode",
];

/// Read a Converse `reasoningContent` block into an IR `Thinking`, or `None` when it carries
/// neither known member (a future union member is left undecoded, not mis-mapped).
///
/// - `reasoningText` (`{ "text", "signature" }`) becomes `Thinking { text, signature }`; Bedrock
///   requires the signature echoed back on a follow-up turn.
/// - `redactedContent` (opaque base64) becomes `Thinking { text: <bytes>, redacted: true }`, so the
///   writer re-emits `redactedContent`; non-Bedrock writers drop it.
///
/// Mirrors anthropic.rs `read_block`'s `"thinking"` arm.
fn read_bedrock_reasoning_block(
    reasoning: &serde_json::Value,
) -> Option<crate::codec::ir::IrBlock> {
    if let Some(reasoning_text) = reasoning.get(REASONING_TEXT) {
        let text = reasoning_text
            .get(keys::TEXT)
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        // IR-18: a busbar provenance envelope restores the original bytes and origin; a genuine
        // Converse signature's origin is derived by the caller from the model id.
        let (signature, signature_origin) = crate::codec::ir::sig_envelope::read_carried_opt(
            reasoning_text
                .get(keys::SIGNATURE)
                .and_then(|s| s.as_str().map(String::from)),
            None,
        );
        return Some(crate::codec::ir::IrBlock::Thinking {
            text,
            signature,
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin,
        });
    }
    if let Some(redacted) = reasoning.get(REDACTED_CONTENT).and_then(|r| r.as_str()) {
        return Some(crate::codec::ir::IrBlock::Thinking {
            text: redacted.to_string(),
            signature: None,
            redacted: true,
            cache_control: None,
            kind: None,
            signature_origin: None,
        });
    }
    None
}

/// Build a `{"reasoningContent": ...}` block from an IR `Thinking`; inverse of
/// `read_bedrock_reasoning_block`. A redacted block re-emits `redactedContent` (bytes in `text`);
/// others re-emit `reasoningText`, with `signature` only when present (Bedrock omits it for an
/// unsigned block). Used by `write_request` and `write_response`.
fn bedrock_reasoning_block(
    text: &str,
    signature: &Option<String>,
    redacted: bool,
) -> serde_json::Value {
    if redacted {
        return serde_json::json!({ (REASONING_CONTENT): { (REDACTED_CONTENT): text } });
    }
    let mut reasoning_text = serde_json::Map::new();
    reasoning_text.insert(keys::TEXT.to_string(), serde_json::json!(text));
    if let Some(sig) = signature {
        reasoning_text.insert(keys::SIGNATURE.to_string(), serde_json::json!(sig));
    }
    serde_json::json!({ (REASONING_CONTENT): { (REASONING_TEXT): serde_json::Value::Object(reasoning_text) } })
}

/// Build a Converse `image` block body (`{ "format", "source": { … } }`) from an `IrImageSource`,
/// or `None` when it has no native form. Converse has only `source.bytes` (base64) and
/// `source.s3Location`, no arbitrary-URL source.
///   - `Base64` becomes `source.bytes`, the subtype mapped onto `ImageFormat` {png, jpeg, gif, webp}
///     (jpg is jpeg). Any other subtype is dropped on the one drop path, never relabelled (design F3).
///   - `Vendor { vendor: "bedrock", value }` re-emits `source.s3Location` (`uri`/`bucketOwner`).
///   - `Url(_)` and a foreign `Vendor` (e.g. a Responses `file_id`) are dropped with a warn rather
///     than emitted as a corrupt `bytes` block.
fn bedrock_image_block(source: &crate::codec::ir::IrImageSource) -> Option<serde_json::Value> {
    match source {
        // A Bedrock vendor reference is an `s3Location` (stored as `{format, s3Location}`); re-emit
        // it. A foreign one has no projection. The caller's own `format` is kept, never invented.
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            let s3_location = value
                .get(S3_LOCATION)
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
            let mut block = serde_json::Map::new();
            if let Some(format_str) = value
                .get(keys::FORMAT)
                .and_then(|f| f.as_str())
                .filter(|s| !s.is_empty())
            {
                block.insert(keys::FORMAT.to_string(), serde_json::json!(format_str));
            }
            block.insert(
                keys::SOURCE.to_string(),
                serde_json::json!({ (S3_LOCATION): s3_location }),
            );
            Some(serde_json::Value::Object(block))
        }
        // No arbitrary-URL source, and a foreign vendor reference has no projection; as base64
        // `bytes` either would corrupt the block. Drop with a warn.
        crate::codec::ir::IrImageSource::Url(_)
        | crate::codec::ir::IrImageSource::Vendor { .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::IMAGE,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping image with no Bedrock Converse projection (URL or foreign vendor ref)"
            );
            None
        }
        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
            // Map the MIME subtype onto `ImageFormat` {png, jpeg, gif, webp} (`image/jpg` is jpeg);
            // any other subtype, or a malformed media type, is dropped.
            let Some(format_str) = crate::codec::ir::image_subtype_if_supported(media_type) else {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::IMAGE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [media_type = %media_type, ],
                    "dropping image block on Bedrock egress: media_type is not one of \
                     image/{{png,jpeg,gif,webp}}, Bedrock Converse's ImageFormat union"
                );
                return None;
            };
            Some(serde_json::json!({
                (keys::FORMAT): format_str,
                (keys::SOURCE): { (keys::BYTES): data }
            }))
        }
    }
}

/// This dialect's name: the `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`] it
/// produces (an `s3Location` source names an object in the caller's AWS account, meaningless elsewhere).
const VENDOR_NAME: &str = "bedrock";

/// A Converse `searchResult` block (`{source, title, content: [{text}], citations: {enabled}}`) to
/// the search-result slot (`IrBlock::search_result`), shared with Anthropic `search_result`. Text
/// parts join with `\n`; the `citations` switch rides verbatim.
fn read_bedrock_search_result(sr: &serde_json::Value) -> crate::codec::ir::IrBlock {
    let source = sr.get(keys::SOURCE).and_then(|v| v.as_str()).unwrap_or("");
    let title = sr.get(keys::TITLE).and_then(|v| v.as_str()).unwrap_or("");
    let body = sr
        .get(keys::CONTENT)
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get(keys::TEXT).and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    crate::codec::ir::IrBlock::search_result(
        source,
        title,
        body,
        sr.get(keys::CITATIONS).cloned(),
        None,
    )
}

/// The search-result slot to a Converse `{"searchResult": {...}}` block (inverse of
/// [`read_bedrock_search_result`]).
fn write_bedrock_search_result(
    sr: &crate::codec::ir::IrSearchResultParts<'_>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(keys::SOURCE.to_string(), serde_json::json!(sr.source));
    obj.insert(keys::TITLE.to_string(), serde_json::json!(sr.title));
    obj.insert(
        keys::CONTENT.to_string(),
        serde_json::json!([{ (keys::TEXT): sr.body }]),
    );
    if let Some(cfg) = sr.citations_config {
        obj.insert(keys::CITATIONS.to_string(), cfg.clone());
    }
    serde_json::json!({ (SEARCH_RESULT_CAMEL): obj })
}

/// Read a Converse `document` / `video` block body into an [`crate::codec::ir::IrBlock::Media`].
/// Both are `{"format": "pdf", "name": "…", "source": {…}}` with a `source` of inline `bytes` or an
/// `s3Location`. `format` is a bare container token, normalized to a mime type (the writer reverses it).
fn bedrock_media_block(
    kind: crate::codec::ir::IrMediaKind,
    value: &serde_json::Value,
) -> crate::codec::ir::IrBlock {
    let format = value
        .get(keys::FORMAT)
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = value
        .get(keys::NAME)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let source = value.get(keys::SOURCE);
    let ir_source = match source
        .and_then(|s| s.get(keys::BYTES))
        .and_then(|b| b.as_str())
    {
        Some(bytes) => crate::codec::ir::IrImageSource::Base64 {
            media_type: bedrock_media_type_for_format(kind, format),
            data: bytes.to_string(),
        },
        // BED-03: a text document (`source.text`, or chunked `source.content[].text`) becomes a
        // base64 text document (the `Base64.data` contract), not a Vendor reference no foreign
        // writer can re-emit.
        None if bedrock_document_text(source).is_some() => {
            crate::codec::ir::IrImageSource::Base64 {
                media_type: bedrock_text_media_type(format),
                data: busbar_contract::media::base64_encode(
                    bedrock_document_text(source).unwrap_or_default().as_bytes(),
                ),
            }
        }
        // An `s3Location` names an object in the caller's AWS account that no other backend can
        // fetch: it rides the opaque `Vendor` escape and only this writer re-emits it.
        None => crate::codec::ir::IrImageSource::Vendor {
            vendor: VENDOR_NAME,
            value: source.cloned().unwrap_or_else(|| serde_json::json!({})),
        },
    };
    // IR-12: a `DocumentBlock` carries `citations: {enabled}` and a free-text `context`; a
    // `VideoBlock` has neither.
    let (citations, context) = if kind == crate::codec::ir::IrMediaKind::Document {
        (
            value
                .get(keys::CITATIONS)
                .and_then(|c| c.get(keys::ENABLED))
                .and_then(|e| e.as_bool()),
            value
                .get(keys::CONTEXT)
                .and_then(|c| c.as_str())
                .map(String::from),
        )
    } else {
        (None, None)
    };
    crate::codec::ir::IrBlock::Media {
        kind,
        source: ir_source,
        name,
        cache_control: None,
        citations,
        context,
    }
}

/// Inline text of a `DocumentSource` that carries text, not bytes: `text`, or `content[]` `{text}`
/// chunks joined by newlines. `None` for a bytes/s3 source.
fn bedrock_document_text(source: Option<&serde_json::Value>) -> Option<String> {
    let source = source?;
    if let Some(t) = source.get(keys::TEXT).and_then(|t| t.as_str()) {
        return Some(t.to_string());
    }
    let parts: Vec<&str> = source
        .get(keys::CONTENT)?
        .as_array()?
        .iter()
        .filter_map(|p| p.get(keys::TEXT).and_then(|t| t.as_str()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Mime type of a text document source: the format's own text type (`md`, `csv`, `html`, `txt`),
/// else `text/plain`.
fn bedrock_text_media_type(format: &str) -> String {
    let m = bedrock_media_type_for_format(crate::codec::ir::IrMediaKind::Document, format);
    if m.starts_with("text/") {
        m
    } else {
        keys::TEXT_PLAIN.to_string()
    }
}

/// Converse's bare container token (`pdf`, `csv`, `mp4`) to a mime type. Unknown tokens fall back to
/// the kind's generic type rather than inventing `application/<token>`.
fn bedrock_media_type_for_format(kind: crate::codec::ir::IrMediaKind, format: &str) -> String {
    let token = format.to_ascii_lowercase();
    if let Some(m) =
        crate::codec::dialect::media_type(&[crate::codec::dialect::DOCUMENT_MEDIA_TYPES], &token)
    {
        return m.to_string();
    }
    match token.as_str() {
        DOC => APPLICATION_MSWORD.to_string(),
        DOCX => APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_WORDPROCESSINGML_DOCUMENT.to_string(),
        XLS => APPLICATION_VND_MS_EXCEL.to_string(),
        XLSX => APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_SPREADSHEETML_SHEET.to_string(),
        FORMAT_MP4 | MOV | WEBM | FLV | MPEG | MPG | WMV | MKV => {
            format!("video/{format}")
        }
        THREE_GP => "video/3gpp".to_string(),
        _ => match kind {
            crate::codec::ir::IrMediaKind::Document => "application/octet-stream".to_string(),
            crate::codec::ir::IrMediaKind::Audio => "audio/mpeg".to_string(),
            crate::codec::ir::IrMediaKind::Video => "video/mp4".to_string(),
        },
    }
}

/// Project an [`crate::codec::ir::IrBlock::Media`] into its Converse content block, or `None` when
/// there is no slot (caller has warned). Converse has `document` and `video` but no audio, each
/// with a closed format union AWS validates, so unmappable formats are coerced or dropped here
/// rather than sent upstream, as for images.
fn bedrock_media_content_block(
    kind: crate::codec::ir::IrMediaKind,
    source: &crate::codec::ir::IrImageSource,
    name: Option<&str>,
    citations: Option<bool>,
    context: Option<&str>,
) -> Option<serde_json::Value> {
    let (wire_key, format) = match kind {
        crate::codec::ir::IrMediaKind::Document => {
            (keys::DOCUMENT, bedrock_document_format(source)?)
        }
        crate::codec::ir::IrMediaKind::Video => (VIDEO, bedrock_video_format(source)?),
        crate::codec::ir::IrMediaKind::Audio => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::AUDIO,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping audio attachment on Bedrock egress: Converse has `document` and `video` \
                 content blocks and NO audio block, so there is no native slot; the block is NOT \
                 emitted"
            );
            return None;
        }
    };
    let wire_source = match source {
        crate::codec::ir::IrImageSource::Base64 { data, .. } => {
            serde_json::json!({ (keys::BYTES): data })
        }
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            value.clone()
        }
        crate::codec::ir::IrImageSource::Url(_)
        | crate::codec::ir::IrImageSource::Vendor { .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::block(kind.as_str()),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [media_kind = kind.as_str(),],
                "dropping attachment on Bedrock egress: Converse has no arbitrary-URL source and \
                 cannot resolve a foreign vendor file handle; the block is NOT emitted"
            );
            return None;
        }
    };
    let mut block = serde_json::Map::new();
    block.insert(keys::FORMAT.to_string(), serde_json::json!(format));
    // Converse requires `name` on a document block; cross-protocol attachments often have none
    // (Gemini `inlineData`), so synthesize one rather than send a block AWS rejects.
    if kind == crate::codec::ir::IrMediaKind::Document {
        block.insert(
            keys::NAME.to_string(),
            serde_json::json!(name.unwrap_or("attachment")),
        );
    } else if let Some(n) = name {
        block.insert(keys::NAME.to_string(), serde_json::json!(n));
    }
    block.insert(keys::SOURCE.to_string(), wire_source);
    // IR-12: citation switch and context ride `DocumentBlock` natively; a `VideoBlock` has none, so
    // they are dropped there (warn when set).
    if kind == crate::codec::ir::IrMediaKind::Document {
        if let Some(c) = context {
            block.insert(keys::CONTEXT.to_string(), serde_json::json!(c));
        }
        if let Some(enabled) = citations {
            block.insert(
                keys::CITATIONS.to_string(),
                serde_json::json!({ (keys::ENABLED): enabled }),
            );
        }
    } else if citations.is_some() || context.is_some() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::DOCUMENT,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [media_kind = kind.as_str(),],
            "dropping attachment citations/context on Bedrock egress: Converse carries them on a \
             document block only"
        );
    }
    Some(serde_json::json!({ wire_key: serde_json::Value::Object(block) }))
}

/// Mime type to Converse's closed `DocumentFormat` union, or `None` (drop with a warn).
fn bedrock_document_format(source: &crate::codec::ir::IrImageSource) -> Option<&'static str> {
    // An s3Location source has no mime; Converse still requires a format, and `pdf` is the usual document.
    let crate::codec::ir::IrImageSource::Base64 { media_type, .. } = source else {
        return Some(PDF);
    };
    let f = match media_type.to_ascii_lowercase().as_str() {
        "application/pdf" => PDF,
        "text/csv" => "csv",
        keys::TEXT_PLAIN => "txt",
        "text/markdown" | "text/x-markdown" => "md",
        "text/html" => "html",
        APPLICATION_MSWORD => DOC,
        APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_WORDPROCESSINGML_DOCUMENT => DOCX,
        APPLICATION_VND_MS_EXCEL => XLS,
        APPLICATION_VND_OPENXMLFORMATS_OFFICEDOCUMENT_SPREADSHEETML_SHEET => XLSX,
        other => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::DOCUMENT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [media_type = %other, ],
                "dropping document attachment on Bedrock egress: the mime type is not a member of \
                 Converse's DocumentFormat union {{pdf,csv,doc,docx,xls,xlsx,html,txt,md}} and AWS \
                 rejects anything else; the block is NOT emitted");
            return None;
        }
    };
    Some(f)
}

/// Mime type to Converse's closed `VideoFormat` union, or `None` (drop with a warn).
fn bedrock_video_format(source: &crate::codec::ir::IrImageSource) -> Option<&'static str> {
    let crate::codec::ir::IrImageSource::Base64 { media_type, .. } = source else {
        return Some(FORMAT_MP4);
    };
    let subtype = media_type
        .to_ascii_lowercase()
        .strip_prefix("video/")
        .map(String::from);
    let f = match subtype.as_deref() {
        Some(FORMAT_MP4) => FORMAT_MP4,
        Some("quicktime") | Some(MOV) => MOV,
        Some(WEBM) => WEBM,
        Some("x-flv") | Some(FLV) => FLV,
        Some(MPEG) | Some(MPG) => MPEG,
        Some("x-ms-wmv") | Some(WMV) => WMV,
        Some("x-matroska") | Some(MKV) => MKV,
        Some("3gpp") => THREE_GP,
        _ => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::VIDEO,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [media_type = %media_type, ],
                "dropping video attachment on Bedrock egress: the mime type is not a member of \
                 Converse's VideoFormat union; the block is NOT emitted");
            return None;
        }
    };
    Some(f)
}

/// Build a Converse prompt-cache marker (`{"cachePoint": {"type": "default"}}`): everything before
/// it in the same array is cached as a prefix. The IR carries the boundary as `cache_control` on a
/// block or tool; the writer emits this marker immediately after it, where Bedrock expects it.
fn bedrock_cache_point() -> serde_json::Value {
    serde_json::json!({ (CACHE_POINT): { (keys::TYPE): "default" } })
}

/// Read `outputConfig.textFormat` (`{type: "json_schema", structure: {jsonSchema: {schema, name,
/// description}}}`) into [`crate::codec::ir::IrResponseFormat`] (BED-08). The schema is a string on
/// this wire; an unparseable one yields no directive.
fn read_bedrock_response_format(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Option<crate::codec::ir::IrResponseFormat> {
    let tf = body.get(OUTPUT_CONFIG_CAMEL)?.get(TEXT_FORMAT)?;
    if tf.get(keys::TYPE).and_then(|t| t.as_str()) != Some(keys::JSON_SCHEMA) {
        return None;
    }
    let js = tf.get(STRUCTURE)?.get(JSON_SCHEMA_CAMEL)?;
    let schema: serde_json::Value = serde_json::from_str(js.get(keys::SCHEMA)?.as_str()?).ok()?;
    Some(crate::codec::ir::IrResponseFormat {
        json: true,
        schema: Some(schema),
        name: js
            .get(keys::NAME)
            .and_then(|n| n.as_str())
            .map(String::from),
        strict: None,
        description: js
            .get(keys::DESCRIPTION)
            .and_then(|d| d.as_str())
            .map(String::from),
    })
}

/// Project [`crate::codec::ir::IrResponseFormat`] into `outputConfig.textFormat` (BED-08), or
/// `None`: `OutputFormat.type` is `json_schema` only, so schema-less JSON and plain text have no form.
fn write_bedrock_text_format(rf: &crate::codec::ir::IrResponseFormat) -> Option<serde_json::Value> {
    if !rf.json {
        return None;
    }
    let schema = rf.schema.as_ref()?;
    let mut js = serde_json::Map::new();
    js.insert(
        keys::SCHEMA.to_string(),
        serde_json::json!(schema.to_string()),
    );
    if let Some(n) = rf.name.as_deref().filter(|s| !s.is_empty()) {
        js.insert(keys::NAME.to_string(), serde_json::json!(n));
    }
    if let Some(d) = rf.description.as_deref().filter(|s| !s.is_empty()) {
        js.insert(keys::DESCRIPTION.to_string(), serde_json::json!(d));
    }
    Some(serde_json::json!({
        (keys::TYPE): keys::JSON_SCHEMA,
        (STRUCTURE): { (JSON_SCHEMA_CAMEL): serde_json::Value::Object(js) }
    }))
}

/// Set `cache_control` on the last block of an IR content vector from a following `cachePoint`, so
/// the boundary survives cross-protocol (the positional `CACHE_POINTS_SENTINEL` stash is cleared on
/// the seam). `Json` blocks carry no `cache_control` and rely on the stash. This is additive: the
/// writer suppresses inline emission when the stash is present, so same-protocol output stays
/// byte-identical (see `write_request`).
fn set_preceding_block_cache_control(blocks: &mut [crate::codec::ir::IrBlock]) {
    if let Some(last) = blocks.last_mut() {
        let cc = Some(crate::codec::ir::CacheControl {
            kind: crate::codec::ir::CacheKind::Ephemeral,
        });
        match last {
            // BED-07: Thinking / Image / Media carry `cache_control` too, so a cachePoint after one is
            // a boundary a foreign dialect can express.
            crate::codec::ir::IrBlock::Text { cache_control, .. }
            | crate::codec::ir::IrBlock::ToolUse { cache_control, .. }
            | crate::codec::ir::IrBlock::ToolResult { cache_control, .. }
            | crate::codec::ir::IrBlock::Thinking { cache_control, .. }
            | crate::codec::ir::IrBlock::Image { cache_control, .. }
            | crate::codec::ir::IrBlock::Media { cache_control, .. } => {
                *cache_control = cc;
            }
            // Json is a tool-result member only; the positional stash carries the marker.
            crate::codec::ir::IrBlock::Json(_)
            | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
        }
    }
}

/// Splice captured `cachePoint` blocks back into a written content array at the original absolute
/// positions, reconstructing the native ordering on a same-protocol passthrough. `entries` are
/// `{ "i", "block" }` records from the `CACHE_POINTS_SENTINEL` object. A record missing a field or
/// with an out-of-range index is skipped (`extra` survives arbitrary hops, and this must never
/// panic on the request path).
///
/// Records apply in ascending index order: the reader recorded indices against the original array,
/// so inserting at each in order reproduces the layout. Insertion clamps to `min(len)` so a stale
/// index lands at the end.
fn splice_cache_points(arr: &mut Vec<serde_json::Value>, entries: &[serde_json::Value]) {
    // (index, block) pairs, sorted so ascending insertion preserves layout.
    let mut pending: Vec<(usize, serde_json::Value)> = Vec::new();
    for entry in entries {
        let Some(idx) = entry.get(keys::I).and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(block) = entry.get(keys::BLOCK) else {
            continue;
        };
        pending.push((idx as usize, block.clone()));
    }
    pending.sort_by_key(|(idx, _)| *idx);
    for (idx, block) in pending {
        let pos = idx.min(arr.len());
        arr.insert(pos, block);
    }
}

/// Concatenate two optional `{ "i", "block" }` slices (e.g. the `cachePoint` and `guardContent`
/// stashes of one array) for a single `splice_cache_points` pass. Both recorded indices against the
/// same original array, so separate passes would shift the second's indices.
fn merge_marker_entries(
    a: Option<&Vec<serde_json::Value>>,
    b: Option<&Vec<serde_json::Value>>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(entries) = a {
        out.extend_from_slice(entries);
    }
    if let Some(entries) = b {
        out.extend_from_slice(entries);
    }
    out
}

/// The declared SigV4 region of an upstream `host` ([`EgressScheme::SigV4`]'s `region_of_host`):
/// [`derive_sigv4_region`], warning when a host names no region and the scope falls back to `us-east-1`.
fn declared_sigv4_region(host: &str) -> Option<&str> {
    let region = derive_sigv4_region(host);
    if region.is_none() {
        tracing::warn!(host = %host, "could not derive AWS region from Bedrock endpoint host; defaulting SigV4 scope to us-east-1 (set a bedrock-runtime[-fips].<region>.amazonaws.com host)");
    }
    region
}

/// Derive the AWS region for the SigV4 scope from a Bedrock endpoint host. A bare
/// `strip_prefix("bedrock-runtime.")` signs for the wrong region on non-vanilla endpoints, which
/// AWS rejects as a 403 `SignatureDoesNotMatch`. This matches the Bedrock service labels (with or
/// without `-fips`) and VPC-interface (`vpce`) fronts, taking the label after the service label:
///
///   - `bedrock-runtime.<region>.amazonaws.com`
///   - `bedrock-runtime-fips.<region>.amazonaws.com`
///   - `bedrock-runtime.<region>.vpce.amazonaws.com`
///   - `vpce-0abc...-1xyz.bedrock-runtime.<region>.vpce.amazonaws.com`
///   - `bedrock.<region>.amazonaws.com` (control-plane label, defensively)
///
/// `Some(region)` only when a service label is found and the next label looks like a region
/// (alphabetic dash-parts then a numeric part, e.g. `us-east-1`, `us-gov-west-1`); else `None`, on
/// which the caller warns and falls back to `us-east-1`. Pure string parsing; no panic.
pub(crate) fn derive_sigv4_region(host: &str) -> Option<&str> {
    // A region token has >= 3 dash parts, leading parts all ASCII-alphabetic and the final part all
    // digits (`us-east-1`, GovCloud `us-gov-west-1`, ISO `us-iso-east-1`), so any future
    // direction/partition segments still parse. Bare labels, 2-part tokens, IP octets and CNAME
    // segments fail. (Requiring exactly 3 parts rejected GovCloud and ISO and fell back to a wrong scope.)
    fn looks_like_region(label: &str) -> bool {
        let parts: Vec<&str> = label.split('-').collect();
        // Need <area>-<direction>-<number> with no empty parts.
        if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
            return false;
        }
        let Some((last, leading)) = parts.split_last() else {
            return false;
        };
        last.bytes().all(|x| x.is_ascii_digit())
            && leading
                .iter()
                .all(|p| p.bytes().all(|x| x.is_ascii_alphabetic()))
    }

    // Walk the dotted labels; after a Bedrock service label, the next one is the region.
    let labels: Vec<&str> = host.split('.').collect();
    for (i, label) in labels.iter().enumerate() {
        if matches!(
            *label,
            "bedrock-runtime" | "bedrock-runtime-fips" | VENDOR_NAME | "bedrock-fips"
        ) {
            if let Some(next) = labels.get(i + 1) {
                if looks_like_region(next) {
                    return Some(next);
                }
            }
        }
    }
    None
}

/// Read a Converse `image` block into an `IrBlock::Image`, or `None` when it has no usable source.
/// The `ImageSource` union has `source.bytes` (base64) and `source.s3Location`
/// (`{"uri","bucketOwner"}`). Base64 reads as `Base64`; an s3Location is carried whole (plus
/// `format`) in `IrImageSource::Vendor { vendor: "bedrock", value }` so `bedrock_image_block`
/// re-emits it same-protocol (a foreign writer drops it). A block with neither yields `None`, not
/// an empty-bytes block.
fn read_bedrock_image_block(image: &serde_json::Value) -> Option<crate::codec::ir::IrBlock> {
    let format_str = image
        .get(keys::FORMAT)
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let source = image.get(keys::SOURCE);

    // Prefer inline base64 `bytes`.
    if let Some(bytes) = source
        .and_then(|s| s.get(keys::BYTES))
        .and_then(|b| b.as_str())
    {
        return Some(crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 {
                media_type: format!("image/{}", format_str),
                data: bytes.to_string(),
            },
            cache_control: None,
            detail: None,
        });
    }

    // Otherwise an `s3Location`, carried as `{format, s3Location}` in the `Vendor` escape.
    if let Some(s3_location) = source.and_then(|s| s.get(S3_LOCATION)) {
        if s3_location.is_object() {
            return Some(crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Vendor {
                    vendor: VENDOR_NAME,
                    value: serde_json::json!({
                        (keys::FORMAT): format_str,
                        (S3_LOCATION): s3_location.clone(),
                    }),
                },
                cache_control: None,
                detail: None,
            });
        }
    }

    None
}

/// A `guardContent` block's content (BED-02): `{text: {text, qualifiers}}` is prompt text and
/// `{image: {format, source}}` a prompt image. The qualifiers have no neutral form and ride only the
/// positional stash. `None` if neither member.
fn guard_content_block(guard: &serde_json::Value) -> Option<crate::codec::ir::IrBlock> {
    if let Some(t) = guard
        .get(keys::TEXT)
        .and_then(|t| t.get(keys::TEXT))
        .and_then(|t| t.as_str())
    {
        return Some(crate::codec::ir::IrBlock::text(t.to_string()));
    }
    guard.get(keys::IMAGE).and_then(read_bedrock_image_block)
}

/// IR indices of the blocks the reader modelled out of a stashed wire block (the `b` member of a
/// `guardContent` / `document` / `video` record), for one message (`Some(m)`) or `system` (`None`).
/// The writer skips emitting these because the verbatim stash splice re-emits them, keeping a
/// same-protocol round-trip byte-identical while a cross-protocol IR (stash cleared) carries them.
fn stashed_ir_indices<'a>(
    stashes: impl IntoIterator<Item = Option<&'a Vec<serde_json::Value>>>,
    msg: Option<usize>,
) -> std::collections::BTreeSet<usize> {
    stashes
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| msg.is_none_or(|m| e.get(keys::M).and_then(|v| v.as_u64()) == Some(m as u64)))
        .filter_map(|e| e.get(B).and_then(|v| v.as_u64()))
        .map(|b| b as usize)
        .collect()
}

/// Normalize Converse `toolConfig.toolChoice` into the IR union: `{"auto":{}}` to `Auto`,
/// `{"any":{}}` to `Required`, `{"tool":{"name":"X"}}` to `Tool{name:"X"}`. Bedrock has no "none".
/// Absent or unrecognized is `None`. Takes the whole `toolConfig` object.
fn read_bedrock_tool_choice(
    tool_config: Option<&serde_json::Value>,
) -> Option<crate::codec::ir::IrToolChoice> {
    let tc = tool_config?.get(TOOL_CHOICE_CAMEL)?.as_object()?;
    if tc.contains_key(keys::AUTO) {
        Some(crate::codec::ir::IrToolChoice::Auto)
    } else if tc.contains_key(keys::ANY) {
        Some(crate::codec::ir::IrToolChoice::Required)
    } else if let Some(tool) = tc.get(keys::TOOL) {
        tool.get(keys::NAME).and_then(|n| n.as_str()).map(|name| {
            crate::codec::ir::IrToolChoice::Tool {
                name: name.to_string(),
            }
        })
    } else {
        None
    }
}

/// Emit the IR tool-choice union in Bedrock's `toolChoice` shape. `IrToolChoice::None` gives
/// `None` (Converse has no "don't call a tool", so `toolChoice` is omitted).
fn write_bedrock_tool_choice(tc: &crate::codec::ir::IrToolChoice) -> Option<serde_json::Value> {
    match tc {
        crate::codec::ir::IrToolChoice::Auto => Some(serde_json::json!({(keys::AUTO): {}})),
        crate::codec::ir::IrToolChoice::Required => Some(serde_json::json!({(keys::ANY): {}})),
        crate::codec::ir::IrToolChoice::Tool { name } => {
            // `toolChoice.tool` is Claude-only on Converse (Titan/Llama reject it). The writer sees
            // only the IR, which has no model (Bedrock carries it in the URL), so it cannot gate;
            // gating needs new `EgressPrep` plumbing. Emit it and warn so a non-Claude target's
            // ValidationException is diagnosable.
            tracing::warn!(
                "emitting toolChoice.tool (force a SPECIFIC tool) on Bedrock egress: this directive \
                 is documented Anthropic-Claude-only on Converse and Bedrock model families that are \
                 not Claude will reject it; the writer has no way to know which model this request \
                 targets"
            );
            Some(serde_json::json!({(keys::TOOL): {(keys::NAME): name}}))
        }
        crate::codec::ir::IrToolChoice::None => None,
    }
}

/// Bedrock stopReason to canonical IR stop reason.
fn stop_reason_map(ward: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match ward {
        keys::END_TURN => S::EndTurn,
        keys::TOOL_USE => S::ToolUse,
        keys::MAX_TOKENS => S::MaxTokens,
        keys::STOP_SEQUENCE => S::StopSequence,
        // Both moderation outcomes fold to `Safety`.
        CONTENT_FILTERED | "guardrail_intervened" => S::Safety,
        // BED-10: the context window filled, which every client dialect reads as `MaxTokens`.
        STOP_MODEL_CONTEXT_WINDOW_EXCEEDED => S::MaxTokens,
        // Output Bedrock could not parse reads as `Other`: the answer is served and billed, whereas
        // `Error` would turn a buffered answer into a billed 502 and a lane fault.
        _ => S::Other,
    }
}

/// Whether a Bedrock model id names a Claude model (`anthropic.` / `claude`, covering bare ids,
/// inference profiles and ARNs). The Converse body carries no family, so the id is the only
/// signal (see `bedrock_signature_origin`).
fn bedrock_model_is_claude(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("anthropic.") || m.contains("claude")
}

/// Which family minted the reasoning signatures of a conversation addressed to `model` (IR-18).
/// Claude gives `Anthropic`; an id naming another provider (`<provider>.<model>`, behind an
/// optional inference-profile prefix or ARN) gives `BedrockOther`. Anything the id does not reveal
/// (no model, a busbar alias, an opaque application-inference-profile ARN) is `None`, never a guess.
fn bedrock_signature_origin(model: Option<&str>) -> Option<crate::codec::ir::IrSignatureOrigin> {
    let model = model?;
    if bedrock_model_is_claude(model) {
        return Some(crate::codec::ir::IrSignatureOrigin::Anthropic);
    }
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let mut parts = id.split('.');
    let first = parts.next().unwrap_or("");
    let provider = match first {
        "us" | "eu" | "apac" | "global" | "us-gov" | "jp" | "au" | "ca" => parts.next(),
        _ => Some(first),
    }?;
    let names_provider = parts.next().is_some_and(|rest| !rest.is_empty())
        && !provider.is_empty()
        && provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    names_provider.then_some(crate::codec::ir::IrSignatureOrigin::BedrockOther)
}

/// Converse `requestMetadata` (string to string) to the typed [`crate::codec::ir::IrRequest::metadata`]
/// (IR-03). Non-string values are skipped. The raw object still rides `extra`, so a same-protocol
/// hop re-emits it verbatim.
fn read_bedrock_request_metadata(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Option<Vec<(String, String)>> {
    let m = body.get(FIELD_REQUEST_METADATA)?.as_object()?;
    Some(
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect(),
    )
}

/// Converse's request-metadata member.
const FIELD_REQUEST_METADATA: &str = "requestMetadata";
/// Converse caps `requestMetadata` at 16 entries.
const REQUEST_METADATA_MAX_ENTRIES: usize = 16;

/// Whether `s` fits a Converse `requestMetadata` key/value: `min..=256` characters from
/// `[a-zA-Z0-9\s:_@$#=/+,-.]` (the service model's pattern). OpenAI metadata allows 512 characters
/// of anything, so a cross-protocol entry can fall outside it.
fn bedrock_request_metadata_fits(s: &str, min: usize) -> bool {
    let n = s.chars().count();
    (min..=256).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || ":_@$#=/+,-.".contains(c))
}

/// The typed [`crate::codec::ir::IrRequest::metadata`] to `requestMetadata` (IR-03). An entry
/// Converse would reject (pattern, length, or past the 16-entry cap) is dropped with a warn and the
/// rest kept, so one bad entry does not cost a 400. `None` when nothing is left.
fn write_bedrock_request_metadata(pairs: &[(String, String)]) -> Option<serde_json::Value> {
    let mut out = serde_json::Map::new();
    for (k, v) in pairs {
        if !bedrock_request_metadata_fits(k, 1) || !bedrock_request_metadata_fits(v, 0) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::METADATA,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [key = %k, ],
                "dropping a metadata entry on Bedrock egress: Converse requestMetadata keys and \
                 values are at most 256 characters of [a-zA-Z0-9 whitespace :_@$#=/+,-.]");
            continue;
        }
        if out.len() == REQUEST_METADATA_MAX_ENTRIES && !out.contains_key(k) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::METADATA,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [key = %k, ],
                "dropping a metadata entry on Bedrock egress: Converse requestMetadata holds at \
                 most 16 entries");
            continue;
        }
        out.insert(k.clone(), serde_json::json!(v));
    }
    (!out.is_empty()).then_some(serde_json::Value::Object(out))
}

/// The refinement a stopReason states beyond its coarse IR reason (IR-16, BED-10):
/// `model_context_window_exceeded` is `MaxTokens` plus
/// [`crate::codec::ir::IrStopDetail::ContextWindowExceeded`]. Every other stopReason states none.
fn stop_detail_map(ward: &str) -> Option<crate::codec::ir::IrStopDetail> {
    (ward == STOP_MODEL_CONTEXT_WINDOW_EXCEEDED)
        .then_some(crate::codec::ir::IrStopDetail::ContextWindowExceeded)
}

/// Converse's `model_context_window_exceeded` stopReason (IR-16, BED-10).
const STOP_MODEL_CONTEXT_WINDOW_EXCEEDED: &str = "model_context_window_exceeded";

/// Canonical IR stop reason (plus its [`crate::codec::ir::IrStopDetail`]) to Bedrock stopReason. A
/// context-window detail is written as `model_context_window_exceeded` (IR-16, BED-10); every
/// other detail has no Converse spelling, so the coarse reason is written.
fn stop_reason_reverse_detailed(
    canonical: crate::codec::ir::IrStopReason,
    detail: Option<&crate::codec::ir::IrStopDetail>,
) -> &'static str {
    match detail {
        Some(crate::codec::ir::IrStopDetail::ContextWindowExceeded) => {
            STOP_MODEL_CONTEXT_WINDOW_EXCEEDED
        }
        _ => stop_reason_reverse(canonical),
    }
}

/// Canonical IR stop reason to Bedrock stopReason (inverse of `stop_reason_map`).
fn stop_reason_reverse(canonical: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match canonical {
        S::EndTurn => keys::END_TURN,
        S::ToolUse => keys::TOOL_USE,
        S::MaxTokens => keys::MAX_TOKENS,
        S::StopSequence => keys::STOP_SEQUENCE,
        S::Safety => CONTENT_FILTERED,
        // BED-09: a refusal is a content-policy stop, spelled `content_filtered`; the text rides the content.
        S::Refusal => CONTENT_FILTERED,
        // error / pause_turn / other become end_turn, not an off-spec value a strict client rejects.
        S::Error | S::PauseTurn | S::Other => keys::END_TURN,
    }
}

/// Bedrock's usage counts, as data. The cache write is `cacheWriteInputTokens` (Anthropic's
/// creation) and the read `cacheReadInputTokens`; both are optional on the wire, so absent maps to
/// `None`, not `Some(0)`. `cacheDetails` is the per-TTL breakdown of the cache write (`{ttl,
/// inputTokens}`, 1h before 5m); the TTLs are separate tiers, each the sum of its entries
/// (`CountRead::ListSum`). A TTL not reported stays `None`, and an unrecognized `ttl` reaches no
/// tier (the `cacheWriteInputTokens` total still carries it). The buffered response, the stream
/// `metadata` frame and truncated-body recovery read this one table.
const USAGE: &[UsageCount] = &[
    (CountSlot::Input, CountRead::Zero(&[INPUT_TOKENS_CAMEL])),
    (CountSlot::Output, CountRead::Zero(&[OUTPUT_TOKENS_CAMEL])),
    (
        CountSlot::CacheWrite,
        CountRead::Opt(&[CACHE_WRITE_INPUT_TOKENS]),
    ),
    (
        CountSlot::CacheRead,
        CountRead::Opt(&[CACHE_READ_INPUT_TOKENS]),
    ),
    (
        CountSlot::CacheWrite5m,
        CountRead::ListSum {
            list: &[CACHE_DETAILS],
            key: TTL,
            value: CACHE_TTL_5M,
            count: INPUT_TOKENS_CAMEL,
        },
    ),
    (
        CountSlot::CacheWrite1h,
        CountRead::ListSum {
            list: &[CACHE_DETAILS],
            key: TTL,
            value: CACHE_TTL_1H,
            count: INPUT_TOKENS_CAMEL,
        },
    ),
];

/// Identity [`read_bedrock_usage`] checks `usage.totalTokens` against
/// ([`crate::codec::ir::UsageIdentityNote::identity`]).
const BEDROCK_USAGE_IDENTITY: &str = "bedrock.usage";

/// A Converse `usage` object (`None` when absent) to IR usage through [`USAGE`].
///
/// Every count in the pinned wire lock (`testing/llm-conformance/wire/bedrock.wire.json`) is
/// ledgered or a slice of one: `inputTokens`, `outputTokens`, `cacheReadInputTokens`,
/// `cacheWriteInputTokens`, with `cacheDetails[].inputTokens` carried as the 5m/1h attribution
/// inside the cache-write class. `totalTokens` is AWS's sum of the four (see the writer's
/// `converse_total_tokens`), never a unit: it is cross-checked against the ledgered classes, a gap
/// is WARN-logged and carried as the usage identity note, and the gap above the four is the open
/// class `unitemized_tokens`. Guardrail policy units ride `trace`: see [`guardrail_units`].
fn read_bedrock_usage(
    usage_obj: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let mut ir = crate::codec::usage_count::read_usage(VENDOR_NAME, usage_obj, USAGE)?;
    // A refused count leaves no gap to ledger: the total is not cross-checked.
    ir.detail.usage_identity_note = (!crate::codec::usage_count::refused_any(usage_obj, USAGE))
        .then(|| {
            crate::codec::usage_count::stated_total_note(
                VENDOR_NAME,
                BEDROCK_USAGE_IDENTITY,
                usage_obj.and_then(|u| u.get(TOTAL_TOKENS_CAMEL)),
                &ir,
            )
        })
        .flatten();
    Ok(ir)
}

/// Members of a Converse `TokenUsage`: a `usage` object naming none of them is not the turn's
/// token usage (a guardrail's `invocationMetrics.usage` names policy units).
const TOKEN_USAGE_MEMBERS: &[&str] = &[
    INPUT_TOKENS_CAMEL,
    OUTPUT_TOKENS_CAMEL,
    TOTAL_TOKENS_CAMEL,
    CACHE_READ_INPUT_TOKENS,
    CACHE_WRITE_INPUT_TOKENS,
];

/// `trace.guardrail` members, as the Converse service model spells them.
const GUARDRAIL: &str = "guardrail";
const GUARDRAIL_INPUT_ASSESSMENT: &str = "inputAssessment";
const GUARDRAIL_OUTPUT_ASSESSMENTS: &str = "outputAssessments";
const GUARDRAIL_INVOCATION_METRICS: &str = "invocationMetrics";

/// Read every guardrail policy-unit count a turn reports, by the open class each is ledgered under.
///
/// A response (or stream `metadata` frame) that ran a guardrail carries
/// `trace.guardrail.inputAssessment.<id>.invocationMetrics.usage` and
/// `trace.guardrail.outputAssessments.<id>[].invocationMetrics.usage`: policy units AWS bills
/// separately from tokens. Each is its own open class
/// ([`crate::codec::ir::open_class::GUARDRAIL_COUNT_CLASSES`]), never folded into a token class,
/// summed over every guardrail and assessment on both sides; a zero sum is no hit.
///
/// A present count that is not a count is refused on its own (bills nothing, warned, never read as
/// zero) while readable ones bill. `null` is absence.
///
/// The assessment's other integers (`guardrailProcessingLatency`, `guardrailCoverage.*`) are not
/// billed units; ledgering the coverage would count the policy units twice.
fn guardrail_units(
    holder: &serde_json::Value,
) -> Result<std::collections::BTreeMap<String, u64>, IrError> {
    let mut units = std::collections::BTreeMap::new();
    let Some(guardrail) = holder.get(TRACE).and_then(|t| t.get(GUARDRAIL)) else {
        return Ok(units);
    };
    let usage_of = |a: &serde_json::Value| {
        a.get(GUARDRAIL_INVOCATION_METRICS)
            .and_then(|m| m.get(keys::USAGE))
            .cloned()
    };
    let input = guardrail
        .get(GUARDRAIL_INPUT_ASSESSMENT)
        .and_then(|m| m.as_object())
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(usage_of);
    let output = guardrail
        .get(GUARDRAIL_OUTPUT_ASSESSMENTS)
        .and_then(|m| m.as_object())
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(usage_of);
    for usage in input.chain(output) {
        for &(count, class) in crate::codec::ir::open_class::GUARDRAIL_COUNT_CLASSES {
            // Each count is judged on its own: an unreadable one is refused and bills nothing.
            match crate::codec::usage_count::billed_count(&usage, count) {
                Ok(n) => crate::codec::ir::open_class::add(&mut units, class, n),
                Err(unreadable) => tracing::warn!(
                    protocol = VENDOR_NAME,
                    field = unreadable.field,
                    spelling = %unreadable.spelling,
                    "guardrail usage count is present but unreadable: REFUSED, it bills nothing \
                     and is never read as zero (#42, #81)"
                ),
            }
        }
    }
    Ok(units)
}

/// The tier that served a Converse answer (`serviceTier.type`), as `IrUsageDetail::service_tier`.
fn read_served_tier(answer: &serde_json::Value) -> Option<String> {
    crate::codec::carry::read_word(
        map::WORDS_SERVICE_TIER,
        answer
            .get(SERVICE_TIER_CAMEL)
            .and_then(|t| t.get(keys::TYPE)),
    )
}

/// The `serviceTier` member for `usage`'s served tier, when the dialect has a word for it.
fn served_tier_member(usage: &crate::codec::ir::IrUsage) -> Option<serde_json::Value> {
    let tier = usage.detail.service_tier.as_deref()?;
    let word = crate::codec::carry::word_out(map::WORDS_SERVICE_TIER, tier)?;
    Some(serde_json::json!({ (keys::TYPE): word }))
}

/// The `CacheTTL` enum's values, as the service model spells them.
const CACHE_TTL_5M: &str = "5m";
const CACHE_TTL_1H: &str = "1h";

/// Write the IR's per-TTL cache-write split onto a Converse `usage` object (inverse of the
/// `cacheDetails` rows in [`USAGE`]), 1h before 5m, only for tiers the IR carries: the spec says
/// `cacheDetails` is empty if no cache creation occurred, so no split means no member.
fn write_cache_details(
    usage_obj: &mut serde_json::Map<String, serde_json::Value>,
    usage: &crate::codec::ir::IrUsage,
) {
    let mut details = Vec::new();
    if let Some(v) = usage.detail.cache_creation_1h_input_tokens {
        details.push(serde_json::json!({ (TTL): CACHE_TTL_1H, (INPUT_TOKENS_CAMEL): v }));
    }
    if let Some(v) = usage.detail.cache_creation_5m_input_tokens {
        details.push(serde_json::json!({ (TTL): CACHE_TTL_5M, (INPUT_TOKENS_CAMEL): v }));
    }
    if !details.is_empty() {
        usage_obj.insert(CACHE_DETAILS.to_string(), serde_json::Value::Array(details));
    }
}

/// Write the IR's cache token fields onto a Converse `usage` object (inverse of the cache rows in
/// [`USAGE`]): `cacheWriteInputTokens` from `cache_creation_input_tokens`, `cacheReadInputTokens`
/// from `cache_read_input_tokens`, only when `Some`. A `None` is omitted, not `0`, so a no-cache
/// round-trip stays byte-identical to native AWS.
fn write_cache_usage(
    usage_obj: &mut serde_json::Map<String, serde_json::Value>,
    usage: &crate::codec::ir::IrUsage,
) {
    if let Some(ccit) = usage.cache_creation_input_tokens {
        usage_obj.insert(CACHE_WRITE_INPUT_TOKENS.to_string(), ccit.into());
    }
    if let Some(crit) = usage.cache_read_input_tokens {
        usage_obj.insert(CACHE_READ_INPUT_TOKENS.to_string(), crit.into());
    }
    // The per-TTL breakdown is priced separately, so carried separately.
    write_cache_details(usage_obj, usage);
}

/// Upper bound on the upstream-controlled ConverseStream `contentBlockIndex` at every stream read
/// site. A hostile backend can send up to `u64::MAX`, which would key downstream per-index state
/// off a pathological value. Real streams use small sequential indices, so larger is malformed and
/// clamped. Mirrors the OpenAI reader's `MAX_TOOL_INDEX` and Cohere's `MAX_TOOL_FRAME_INDEX`.
const MAX_CONTENT_BLOCK_INDEX: u64 = 1023;

/// Read `contentBlockIndex` off a stream frame (0 when absent or non-numeric), clamped to
/// `MAX_CONTENT_BLOCK_INDEX`. Shared by all three stream read sites.
fn clamp_content_block_index(data: &serde_json::Value) -> usize {
    data.get(CONTENT_BLOCK_INDEX)
        .and_then(|i| i.as_u64())
        .unwrap_or(0)
        .min(MAX_CONTENT_BLOCK_INDEX) as usize
}

#[derive(Clone)]
pub struct BedrockReader;

/// Bedrock-ingress per-stream framing state machine. A native ConverseStream ends with a
/// `messageStop` frame followed by exactly one `metadata` (usage) frame, but the IR carries one
/// combined `MessageDelta{stop_reason, usage}` and a foreign backend can split them. This fans the
/// combined delta into a stop-only delta (`messageStop`) plus, when usage rode with the stop, a
/// usage-only delta (`metadata`); otherwise it defers the metadata to a trailing usage-only delta
/// (OpenAI `include_usage`) or, if none arrives, to the finish-time flush. `emitted`/`pending`
/// enforce the one-metadata invariant. Built per stream via [`BedrockWriter::new_stream_framing`];
/// Bedrock ingress only.
#[derive(Default)]
struct BedrockStreamFraming {
    /// Whether a `metadata` frame was already emitted: suppresses a duplicate usage-only delta and the finish flush.
    emitted: bool,
    /// Set when a combined stop-delta had all-zero usage so `metadata` was deferred for a trailing
    /// usage-only delta. If none arrives (default OpenAI streaming), `on_finish` flushes one
    /// zero-usage `metadata` so the stream is never missing its terminal frame.
    pending: bool,
}

impl super::proto_codec::StreamFraming for BedrockStreamFraming {
    // Usage rides a separate `metadata` frame (`on_combined_stop_delta` / `on_usage_only_delta` /
    // `on_finish`), so this must not use the generic terminal-usage deferral.
    fn folds_terminal_usage(&self) -> bool {
        false
    }

    fn abort_exception_type(&self) -> Option<&'static str> {
        // A native ConverseStream abort emits a modeled exception frame; `InternalServerException`
        // is the generic server fault. Keeps the wire exception name in this module, not the translator.
        Some(EXC_INTERNAL_SERVER)
    }

    fn inject_streaming_metrics(
        &self,
        event_type: &str,
        data: &mut serde_json::Value,
        elapsed_ms: Option<u64>,
    ) {
        // A native `metadata` frame carries `metrics.latencyMs`, which the service model marks
        // required. Inject the elapsed time since the first byte was fed; an existing value (a
        // same-protocol upstream's own) is left alone, and unavailable timing emits `0` rather than
        // dropping the member. The writer leaves `metrics` off so this is its single source.
        if event_type != ET_METADATA {
            return;
        }
        ensure_metrics(data, elapsed_ms);
    }

    fn on_combined_stop_delta(
        &mut self,
        stop_reason: crate::codec::ir::IrStopReason,
        stop_sequence: Option<String>,
        usage: &crate::codec::ir::IrUsage,
    ) -> Option<Vec<crate::codec::ir::IrStreamEvent>> {
        // Frame 1: stop-only delta, `messageStop` (usage, if any, rides frame 2).
        let mut events = vec![crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(stop_reason),
            stop_sequence: stop_sequence.clone(),
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        }];
        // Frame 2: `metadata`. A native stream emits exactly one, so emit it only if real usage rode
        // with the stop (native Bedrock, or any egress bundling usage into the stop delta). All-zero
        // usage means an OpenAI `include_usage` stop chunk whose tokens arrive in a trailing
        // usage-only delta: defer, never emit a zero-usage frame. Guarded on `!self.emitted` so a
        // second combined stop-delta from a hostile backend cannot produce a second `metadata`;
        // real streams emit one stop-delta, so output is byte-identical. Cache-only usage counts
        // too: a full cache hit has zero input/output tokens but non-zero cache tokens, and
        // omitting them under-reported cache tokens.
        let has_usage = usage.input_tokens != 0
            || usage.output_tokens != 0
            || usage.cache_read_input_tokens.unwrap_or(0) != 0
            || usage.cache_creation_input_tokens.unwrap_or(0) != 0;
        if !self.emitted {
            if has_usage {
                events.push(crate::codec::ir::IrStreamEvent::MessageDelta {
                    stop_reason: None,
                    stop_sequence,
                    usage: usage.clone(),
                    stop_detail: None,
                });
                self.emitted = true;
                self.pending = false;
            } else {
                // Deferred: the stop carried no usage. A trailing usage-only delta emits the
                // metadata if it arrives; in default OpenAI streaming it never does, so mark it
                // pending for `on_finish` to flush one zero-usage frame. A native stream always
                // ends with metadata; its absence is a proxy tell and loses token accounting.
                self.pending = true;
            }
        }
        Some(events)
    }

    fn on_usage_only_delta(&mut self) -> Option<bool> {
        // A usage-only delta (`stop_reason: None`) becomes `metadata`: the trailing OpenAI
        // `include_usage` chunk or a native usage frame. At most once, suppressed if `metadata`
        // already rode with the stop.
        if self.emitted {
            return Some(false);
        }
        self.emitted = true;
        self.pending = false; // the deferral is now resolved
        Some(true)
    }

    fn on_finish(&mut self) -> Option<crate::codec::ir::IrStreamEvent> {
        // If a combined stop-delta deferred `metadata` and no usage-only delta arrived (default
        // OpenAI streaming), flush one zero-usage `metadata` now.
        if !self.pending || self.emitted {
            return None;
        }
        self.emitted = true;
        self.pending = false;
        Some(crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: None,
            stop_sequence: None,
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        })
    }
}

/// Per-stream set of IR block indices this writer opened and so owes a closing `contentBlockStop`
/// (Text / ToolUse / Thinking, not `Image`, whose `BlockStart` maps to `None`). Tracking does not
/// mean a `contentBlockStart` was emitted: only `ToolUse` has a start frame; Text and Thinking open
/// implicitly on their first `contentBlockDelta` (`ContentBlockStart$start` models only
/// `toolUse`). `BlockStop` carries only the index, so without this it closed every index and
/// emitted an orphan `contentBlockStop` for a block the client never saw opened. Mirrors
/// `CohereWriter`'s guard (`cohere/mod.rs`).
#[derive(Clone)]
pub struct BedrockWriter {
    open_block_indices: StreamCell<std::collections::BTreeSet<usize>>,
}

/// Value-namespace constructor for [`BedrockWriter`], mirroring `CohereWriter`'s. `protocol_for`
/// builds a fresh `Protocol` per stream and each use inlines an independent empty set, so state
/// cannot leak across streams. `declare_interior_mutable_const` is suppressed deliberately: a
/// shared `static` would leak one stream's open indices into another.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const BedrockWriter: BedrockWriter = BedrockWriter {
    open_block_indices: StreamCell::new(std::collections::BTreeSet::new()),
};

impl BedrockWriter {
    /// Record that block `index` was opened and owes a `contentBlockStop` (Text/Thinking open
    /// implicitly, ToolUse emits a start). A poisoned lock is a no-op.
    fn mark_block_open(&self, index: usize) {
        self.open_block_indices.lock().insert(index);
    }

    /// True and forget `index` if the block was opened (its `BlockStop` must emit
    /// `contentBlockStop`); false if never opened (e.g. `Image`), then `BlockStop` emits nothing.
    /// A poisoned lock gives `false`.
    fn take_block_open(&self, index: usize) -> bool {
        self.open_block_indices.lock().remove(&index)
    }
}

/// Wrap one non-stream `IrResponse` into a ConverseStream binary `eventstream`
/// (`application/vnd.amazon.eventstream`), for a bedrock-ingress client that asked for
/// `ConverseStream` when a cross-protocol upstream answered with a buffered 2xx (a plain JSON body
/// is undecodable by the SDK). It synthesizes the native sequence: `messageStart`, per block a
/// `contentBlockStart` + `contentBlockDelta`(s) + `contentBlockStop`, then `messageStop` and a
/// trailing `metadata`. Events go through the same writer as the live path and are encoded via
/// `eventstream::encode_frame`. Never panics: a frame that fails to serialize is skipped.
pub fn bedrock_response_to_eventstream(
    host: &dyn busbar_contract::codec::CodecHost,
    ir: &crate::codec::ir::IrResponse,
    elapsed_ms: Option<u64>,
) -> Vec<u8> {
    use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStreamEvent, IrUsage};
    let writer = protocol();
    let writer = writer.writer();
    let mut out: Vec<u8> = Vec::new();
    // Render one IR event through the bedrock writer and append the frame (some events have no
    // Bedrock analog). `write_response_events`, not the single-frame method: a multi-citation delta
    // frames as one `citation` delta per citation, as a native stream does.
    let push = |ev: &IrStreamEvent, out: &mut Vec<u8>| {
        for (event_type, mut payload) in writer.write_response_events(host, ev) {
            // A native `metadata` frame always carries `metrics.latencyMs`; the writer's
            // `MessageDelta` arm omits it and the live StreamTranslate path injects it
            // (`proto::mod.rs`). This buffered path bypasses StreamTranslate, so inject it here with
            // the request's elapsed time (emitted even when timing is unavailable, see `ensure_metrics`).
            if event_type == ET_METADATA {
                ensure_metrics(&mut payload, elapsed_ms);
            }
            if let Ok(bytes) = crate::codec::json::to_vec(&payload) {
                out.extend_from_slice(&crate::codec::eventstream::encode_frame(
                    &event_type,
                    &bytes,
                ));
            }
        }
    };

    // messageStart
    push(
        &IrStreamEvent::MessageStart {
            role: ir.role,
            usage: None,
            id: None,
            created: None,
            model: ir.model.clone(),
        },
        &mut out,
    );

    // Per block: contentBlockStart, contentBlockDelta(s), contentBlockStop, mirroring
    // `read_response_events`. `index` advances only when a block emits frames (not `enumerate()`):
    // ToolResult/Image/Json emit nothing and would leave a gap a native stream never has.
    let mut index = 0usize;
    for block in ir.content.iter() {
        match block {
            IrBlock::Text {
                text, citations, ..
            } => {
                push(
                    &IrStreamEvent::BlockStart {
                        index,
                        block: IrBlockMeta::Text,
                        refusal: false,
                    },
                    &mut out,
                );
                push(
                    &IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::TextDelta(text.clone()),
                    },
                    &mut out,
                );
                // BED-12: citations ride the block's `contentBlockIndex` as `citation` deltas, as the
                // live stream emits them.
                if !citations.is_empty() {
                    push(
                        &IrStreamEvent::BlockDelta {
                            index,
                            delta: IrDelta::CitationsDelta(citations.clone()),
                        },
                        &mut out,
                    );
                }
                push(&IrStreamEvent::BlockStop { index }, &mut out);
                index += 1;
            }
            IrBlock::ToolUse {
                id, name, input, ..
            } => {
                push(
                    &IrStreamEvent::BlockStart {
                        index,
                        block: IrBlockMeta::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                        },
                        refusal: false,
                    },
                    &mut out,
                );
                push(
                    &IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::InputJsonDelta(input.to_string()),
                    },
                    &mut out,
                );
                push(&IrStreamEvent::BlockStop { index }, &mut out);
                index += 1;
            }
            // A Thinking block streams as `contentBlockDelta.reasoningContent` frames closed by
            // `contentBlockStop`, with no `contentBlockStart` (`ContentBlockStart$start` models only
            // `toolUse`). The writer maps the Thinking start to None (marking the block open so its
            // stop fires) and re-emits each Thinking/Signature/RedactedReasoning delta. Same event
            // sequence as the live path.
            IrBlock::Thinking {
                text,
                signature,
                redacted,
                kind,
                ..
            } => {
                push(
                    &IrStreamEvent::BlockStart {
                        index,
                        block: IrBlockMeta::Thinking { kind: *kind },
                        refusal: false,
                    },
                    &mut out,
                );
                if *redacted {
                    // Opaque encrypted reasoning: `text` holds the bytes, carried in one delta.
                    push(
                        &IrStreamEvent::BlockDelta {
                            index,
                            delta: IrDelta::RedactedReasoningDelta(text.clone()),
                        },
                        &mut out,
                    );
                } else {
                    push(
                        &IrStreamEvent::BlockDelta {
                            index,
                            delta: IrDelta::ThinkingDelta(text.clone()),
                        },
                        &mut out,
                    );
                    if let Some(sig) = signature {
                        push(
                            &IrStreamEvent::BlockDelta {
                                index,
                                delta: IrDelta::SignatureDelta(sig.clone()),
                            },
                            &mut out,
                        );
                    }
                }
                push(&IrStreamEvent::BlockStop { index }, &mut out);
                index += 1;
            }
            // ToolResult/Image/Json have no ConverseStream delta frame here; skipped without
            // advancing `index`. Listed explicitly so a new `IrBlock` variant fails to compile.
            IrBlock::ToolResult { .. }
            | IrBlock::Image { .. }
            | IrBlock::Media { .. }
            | IrBlock::Json(_)
            | IrBlock::HostedToolRecord { .. } => {}
        }
    }

    // `messageStop` (stop reason) then `metadata` (usage): the writer's `MessageDelta` arm maps a
    // stop-bearing delta to the first and a usage-only delta to the second. Default the stop reason
    // from the content, not always `end_turn`: if the IR has a ToolUse block but no stop_reason,
    // use `tool_use` (as native Converse does) so an SDK consumer re-invokes the tool; else `end_turn`.
    let default_stop_reason = if ir
        .content
        .iter()
        .any(|b| matches!(b, IrBlock::ToolUse { .. }))
    {
        crate::codec::ir::IrStopReason::ToolUse
    } else {
        crate::codec::ir::IrStopReason::EndTurn
    };
    push(
        &IrStreamEvent::MessageDelta {
            stop_reason: ir.stop_reason.or(Some(default_stop_reason)),
            usage: IrUsage::default(),
            stop_sequence: None,
            // IR-16: the refinement rides the stop frame, so buffered equals stream.
            stop_detail: ir.stop_detail.clone(),
        },
        &mut out,
    );
    push(
        &IrStreamEvent::MessageDelta {
            stop_reason: None,
            usage: ir.usage.clone(),
            stop_sequence: None,
            stop_detail: None,
        },
        &mut out,
    );
    out
}

/// A `metrics` object carrying `latencyMs`, the one member the Converse `metrics` shape has.
fn metrics_object(latency_ms: u64) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    metrics.insert(
        FIELD_LATENCY_MS.to_string(),
        serde_json::Value::from(latency_ms),
    );
    serde_json::Value::Object(metrics)
}

/// True when `value` already has an integer `metrics.latencyMs` (the upstream's own, preferred).
fn has_valid_metrics(value: &serde_json::Value) -> bool {
    value
        .get(FIELD_METRICS)
        .and_then(|m| m.get(FIELD_LATENCY_MS))
        .is_some_and(|ms| ms.is_u64() || ms.is_i64())
}

/// Ensure a Converse JSON object carries `metrics.latencyMs`. An upstream `metrics` is kept;
/// otherwise busbar's measured latency is written. `metrics` is required on the response and the
/// `metadata` event, so with no measurement it is emitted as `0` rather than dropped. Non-objects
/// are left alone.
pub fn ensure_metrics(value: &mut serde_json::Value, elapsed_ms: Option<u64>) {
    if has_valid_metrics(value) {
        return;
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            FIELD_METRICS.to_string(),
            metrics_object(elapsed_ms.unwrap_or(0)),
        );
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
#[path = "tests/usage_float_tests.rs"]
mod usage_float_tests;

#[cfg(test)]
#[path = "tests/usage_census_tests.rs"]
mod usage_census_tests;

#[cfg(test)]
#[path = "tests/ir_mapping_tests.rs"]
mod ir_mapping_tests;

#[cfg(test)]
#[path = "tests/ir_mapping_structured_tests.rs"]
mod ir_mapping_structured_tests;

/// IR mapping: the typed IR slots and lane capabilities (BED-10, IR-03/10/11/12/18, IR-09, LaneCaps).
#[cfg(test)]
#[path = "tests/ir_slot_wiring_tests.rs"]
mod ir_slot_wiring_tests;

#[cfg(test)]
#[path = "tests/ir_round3_tests.rs"]
mod ir_round3_tests;

#[cfg(test)]
#[path = "tests/df_map_audit_tests.rs"]
mod df_map_audit_tests;

const CONTENT_POLICY: &str = "contentPolicy";
const FILTERS: &str = "filters";
const DETECTED: &str = "detected";
const ACTION: &str = "action";
const ACTION_BLOCKED: &str = "BLOCKED";

/// A guardrail trace's content-policy filters to IR safety verdicts: one per filter that
/// `detected` or blocked, `type` as the category. `confidence` and `filterStrength` are AWS's own
/// scale and do not cross; other policies (topics, words, PII, grounding) have no counterpart.
fn read_guardrail_verdicts(
    trace: Option<&serde_json::Value>,
) -> Vec<crate::codec::ir::IrSafetyVerdict> {
    let Some(g) = trace.and_then(|t| t.get(GUARDRAIL)) else {
        return Vec::new();
    };
    let input = g
        .get(GUARDRAIL_INPUT_ASSESSMENT)
        .and_then(|m| m.as_object())
        .into_iter()
        .flat_map(|m| m.values());
    let output = g
        .get(GUARDRAIL_OUTPUT_ASSESSMENTS)
        .and_then(|m| m.as_object())
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|a| a.as_array())
        .flatten();
    input
        .chain(output)
        .filter_map(|a| a.get(CONTENT_POLICY)?.get(FILTERS)?.as_array())
        .flatten()
        .filter_map(|f| {
            let blocked = f.get(ACTION).and_then(|a| a.as_str()) == Some(ACTION_BLOCKED);
            let detected = f.get(DETECTED).and_then(|d| d.as_bool()).unwrap_or(blocked);
            (detected || blocked).then_some(())?;
            Some(crate::codec::ir::IrSafetyVerdict {
                category: f.get(keys::TYPE)?.as_str()?.to_string(),
                flagged: true,
                blocked,
            })
        })
        .collect()
}

/// The search-result slot: Converse `searchResult` <-> Anthropic `search_result`.
#[cfg(test)]
#[path = "tests/search_result_slot_tests.rs"]
mod search_result_slot_tests;
