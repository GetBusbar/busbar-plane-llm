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
// Reached relatively so it resolves both standalone and netted into core.
#[allow(unused_imports)]
// used standalone; redundant with the `busbar_contract::protocol::*` glob when netted into core
use super::proto_codec::*;
use crate::codec::usage_count::{CountRead, CountSlot, UsageCount};
use busbar_contract::ir::egress_prep::LaneCaps;
// Named explicitly so it binds to this crate's `proto_codec` rather than the glob.
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

// One spelling per wire word; words shared with other dialects live in `crate::codec::keys`.

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

/// Build this dialect's wire codec (a fresh instance per resolution).
pub fn protocol() -> Protocol {
    Protocol::new(VENDOR_NAME, BedrockReader, BedrockWriter)
}

/// Router detection: the SigV4 `Authorization: AWS4-HMAC-SHA256…` signature is the tightest claim
/// of any dialect (1, unambiguous whatever the path), then `/converse` (12) and
/// `/model/{id}/invoke` (13). Lower strength binds tighter.
fn claims(
    h: busbar_contract::protocol::HeadFields<'_>,
    path: &str,
) -> Option<busbar_contract::protocol::ClaimStrength> {
    use busbar_contract::protocol::{head_field, ClaimStrength};
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

/// Headerless detection: a `/model/{id}/converse[-stream]` path names Bedrock (30). The
/// `/converse` suffix is load-bearing: a non-Converse `/model/…` path must not wear a Bedrock
/// envelope.
fn residual_claims(path: &str) -> Option<busbar_contract::protocol::ClaimStrength> {
    if path.starts_with("/model/")
        && (path.ends_with("/converse") || path.ends_with("/converse-stream"))
    {
        return Some(busbar_contract::protocol::ClaimStrength(30));
    }
    None
}

/// Response-side untranslatable metadata: the guardrail assessment under a top-level `trace`
/// (`trace.guardrail`), present only when the request asked. Reported so the cross-protocol seam
/// logs the drop; it is an AWS account artifact no other protocol can carry.
fn vendor_response_metadata(body: &serde_json::Value) -> Vec<&'static str> {
    [TRACE]
        .into_iter()
        .filter(|k| body.get(k).is_some())
        .collect()
}

/// Bedrock's protocol declaration: the only one declaring SigV4 ingress auth and a non-SSE
/// streaming content type.
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
    // SigV4 is this dialect's egress scheme, declared as data: the service it signs for, the
    // region as a pure function of the endpoint host (`us-east-1` fallback), and the content type
    // the signature covers. The kernel holds the lane credential (`ACCESS:SECRET[:SESSION]`) and
    // signs under a teller-minted grant, so the secret never passes through this plane. Never
    // lane-constant: every signature covers the body, the time and the path.
    egress_auth_headers: None,
    egress_auth_lane_constant: false,
    egress_scheme: Some(EgressScheme::SigV4 {
        service: VENDOR_NAME,
        region_of_host: declared_sigv4_region,
        default_region: "us-east-1",
        content_type: busbar_contract::protocol::APPLICATION_JSON,
    }),
    // The model is in the URL (`/model/{model_id}/converse`, `/converse-stream`, `/invoke`); the
    // dialect registers its arrival through `PATH_INGRESS`, and `has_model_in_url: true` below is
    // what the boot parity assert pairs with that registration.
    stream_usage_requires_opt_in: false,
    requires_max_tokens: false,
    stop_sequence_cap: None,
    cache_markers_model_gated: true,
    fills_thought_signature: false,
    frame_after_message_start: None,
    reshapes_body_at_path_base: false,
    max_cache_control_breakpoints: None,
    // 400 with `ServiceQuotaExceededException`: a spent budget
    // is not transient, and AWS SDKs retry a 429 `ThrottlingException` with backoff before failing.
    quota_exceeded_status: busbar_contract::http::StatusCode::BAD_REQUEST.as_u16(),
    ingress_is_eventstream: true,
    emits_sse_done_terminator: false,
    max_citations_per_delta: Some(1),
    // Bedrock is reached via boto3/botocore. Re-verify per release
    // (`test_egress_ua_versions_are_pinned_and_present` guards drift). Written on a translated
    // route, on a same-dialect route whose caller sent no user-agent, and on the health probe; a
    // same-dialect caller's own user-agent replaces it.
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
    // Botocore/boto3 sends the binary eventstream `Accept` on a `ConverseStream` call (the same
    // value as the streaming Content-Type); non-stream is `application/json` like every dialect.
    egress_stream_accept: APPLICATION_VND_AMAZON_EVENTSTREAM,
    // No list-models surface: Bedrock's model discovery is not a `/v1/models` GET.
    models_list_envelope: None,
    claims: Some(claims),
    residual_claims: Some(residual_claims),
    residual_default: false,
    vendor_response_metadata: Some(vendor_response_metadata),
    // No wire-fingerprint header disambiguates Bedrock on the shared list-models surface (its
    // `AWS4-HMAC-SHA256` credential must NOT steer a models-list GET; the OpenAI residual wins).
    list_models_fingerprint_headers: &[],
    static_headers: &[],
};

/// This dialect's registration (listed in `crate::codec::DIALECTS`).
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
/// `__type` for typed-exception dispatch. Surfaced externally only via the writer vtable.
const HDR_AMZN_REQUEST_ID: &str = "x-amzn-requestid";
const HDR_AMZN_ERROR_TYPE: &str = "x-amzn-errortype";

/// The four Converse exception names used in both the request-level error path
/// (`error_kind_to_bedrock_type`) and the stream-exception path (`bedrock_stream_exception_for`).
const EXC_THROTTLING: &str = "ThrottlingException";
const EXC_VALIDATION: &str = "ValidationException";
const EXC_SERVICE_UNAVAILABLE: &str = "ServiceUnavailableException";
const EXC_INTERNAL_SERVER: &str = "InternalServerException";

/// The binary framing content-type for Converse streaming; both the response `Content-Type` and the
/// egress `Accept` for a `ConverseStream` call must carry exactly this value.
const APPLICATION_VND_AMAZON_EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

/// The Bedrock-side spelling of the "overloaded" error type in AWS's `__type` field
/// (`ServiceUnavailableException` maps back to this on a round-trip), distinct from busbar's own
/// `KIND_OVERLOADED`. Both map to `ServiceUnavailableException` via `error_kind_to_bedrock_type`.
const ERR_TYPE_OVERLOADED: &str = busbar_contract::protocol::ERR_TYPE_OVERLOADED;

/// The declared refusals (`refusals.json`, shipped with the plane): each row names the busbar
/// refusal kinds it answers, the exception and its HTTP status. The runtime answers a listed kind
/// with exactly the row's exception ([`error_kind_to_bedrock_type`]), and the conformance rig reads
/// the same table, so the plane emits only what it declares.
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

/// Map busbar's generic error `kind` to the Converse exception name carried in `__type`. The
/// Converse error model is a closed set (`AccessDeniedException`, `InternalServerException`,
/// `ModelErrorException`, `ModelNotReadyException`, `ModelTimeoutException`,
/// `ResourceNotFoundException`, `ServiceUnavailableException`, `ThrottlingException`,
/// `ValidationException`), plus InvokeModel's `ServiceQuotaExceededException`, which an exhausted
/// quota or budget answers: it is not retryable, where every AWS SDK retries `ThrottlingException`
/// and a spent budget does not come back by retrying. A kind with no counterpart falls back to
/// `ValidationException` so `__type` is always a real AWS exception name. This is the inverse of
/// the `__type` token `extract_error` reads, so a same-protocol error round-trips its type.
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
        busbar_contract::protocol::KIND_TIMEOUT | "model_timeout" => "ModelTimeoutException",
        busbar_contract::protocol::KIND_OVERLOADED
        | ERR_TYPE_OVERLOADED
        | "service_unavailable"
        | "unavailable" => EXC_SERVICE_UNAVAILABLE,
        busbar_contract::protocol::KIND_API_ERROR
        | "internal_error"
        | busbar_contract::protocol::KIND_SERVER_ERROR => EXC_INTERNAL_SERVER,
        // No native counterpart: the generic client-error exception, a name an SDK can decode.
        _ => EXC_VALIDATION,
    }
}

/// Mint a UUID-v4-shaped request id (`8-4-4-4-12` lowercase hex) for the `x-amzn-RequestId` header
/// a native response always carries, success and error, stream and non-stream (an absent header
/// is a proxy tell). Uses the OS CSPRNG; `None` (the caller omits the header) if entropy is
/// unavailable, and it must never panic on the request path. Every path reaches this through the
/// writer vtable, so there are no private copies.
pub fn synth_amzn_request_id(host: &dyn busbar_contract::codec::CodecHost) -> Option<String> {
    let mut buf = [0u8; 16];
    if !host.fill_entropy(&mut buf) {
        return None;
    }
    // RFC 4122 v4 layout (version + variant bits) so the value is a well-formed UUID.
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    // One allocation for the 32-char lowercase hex string (was 17+ via per-byte `format!`).
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

/// Attach the `x-amzn-RequestId` and `x-amzn-errortype` headers a native error response always
/// carries. `x-amzn-errortype` mirrors the body `__type` (via `error_kind_to_bedrock_type`) so
/// header and body agree. The single source of those headers, reached only through the
/// `BedrockWriter::attach_error_response_headers` vtable method. Best-effort: if entropy or header
/// encoding fails that header is skipped rather than panicking on the request path.
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

/// Map a mid-stream `IrError` to the ConverseStream output-union member name the SDK's stream
/// decoder recognizes, plus the message.
///
/// Distinct from `error_kind_to_bedrock_type`: the stream's modeled error events are a smaller
/// union of exactly five shapes (`InternalServerException`, `ModelStreamErrorException`,
/// `ValidationException`, `ThrottlingException`, `ServiceUnavailableException`). Request-level
/// names such as `ModelTimeoutException`, `AccessDeniedException` and
/// `ServiceQuotaExceededException` are not members, so an SDK would treat them as unmodeled events
/// and never raise the typed exception (a tell). Every class folds onto a legal member:
///
/// - `RateLimit` → `ThrottlingException`
/// - `Overloaded` → `ServiceUnavailableException`
/// - `ClientError` / `ContextLength` → `ValidationException`
/// - `Timeout` → `ModelStreamErrorException`
/// - `Auth` / `Billing` / `ServerError` / `Network` → `InternalServerException`
///
/// Each class is matched explicitly so a new `StatusClass` variant fails to compile. Shared by
/// `write_response_exception` and the `write_response_event` Error arm. The message prefers the
/// upstream's `provider_signal`, falling back to the exception name.
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

/// `extra` key under which the Bedrock reader stashes the positions of native `cachePoint` content
/// blocks (prompt-cache markers, `{"cachePoint": {"type": "default"}}`) inside the `system` array
/// and each message's `content` array.
///
/// A `cachePoint` has no IR counterpart, so without this capture a same-protocol passthrough would
/// drop it, disabling prompt caching the caller requested and re-billing the cached prefix every
/// turn. It is a Bedrock-native marker with no cross-protocol meaning, so it survives a
/// same-protocol round-trip and is dropped where `extra` is cleared at the seam.
///
/// The stash records each block's original absolute index so `write_request` can splice it back
/// at the same position. Shape:
/// ```json
/// {
///   "system":   [ { "i": <usize>, "block": <cachePoint value> }, ... ],
///   "messages": [ { "m": <usize>, "i": <usize>, "block": <cachePoint value> }, ... ]
/// }
/// ```
/// The `__busbar` prefix avoids any real Bedrock key, and `write_request` consumes it, so it never
/// appears on the wire.
const CACHE_POINTS_SENTINEL: &str = "__busbar_bedrock_cache_points";

/// `extra` key under which the Bedrock reader stashes the positions of native `guardContent` blocks
/// (inline Guardrails markers, `{"guardContent": {"text": {"text": ..., "qualifiers": [...]}}}` or
/// `{"guardContent": {"image": {...}}}`) inside the `system` array and each message's `content`.
///
/// A `guardContent` has no IR counterpart: its qualifiers (`grounding_source` / `query` /
/// `guard_content`) tell Guardrails which spans to evaluate and are not expressible as plain Text.
/// Without this capture a same-protocol passthrough would drop it, disabling the inline
/// classification the caller requested and diverging from a direct AWS call. Bedrock-native, so it
/// survives a same-protocol round-trip and is dropped where `extra` is cleared at the seam.
///
/// Same shape as the `cachePoint` stash (`{ "i", "block" }` / `{ "m", "i", "block" }`), spliced
/// back by the shared `splice_cache_points`; the `__busbar` prefix avoids real keys and
/// `write_request` consumes it, so it never appears on the wire.
const GUARD_CONTENT_SENTINEL: &str = "__busbar_bedrock_guard_content";

/// `extra` key under which the Bedrock reader stashes the positions of native `document` and
/// `video` content blocks (`{"document": {...}}` / `{"video": {...}}`) inside each message's
/// `content` array.
///
/// These have no IR counterpart: their format plus `bytes`/`s3Location` source is not expressible
/// by any neutral block. Without this capture a same-protocol passthrough would drop them (a PDF
/// uploaded for analysis would vanish). Bedrock-native, so they survive a same-protocol round-trip
/// and are dropped where `extra` is cleared at the seam.
///
/// Same `{ "m", "i", "block" }` shape as the `guardContent` stash, spliced back at the original
/// index by the shared `splice_cache_points`; `write_request` consumes it, so it never reaches the
/// wire.
const DOC_VIDEO_SENTINEL: &str = "__busbar_bedrock_doc_video";

/// ConverseStream event-type names, the discriminator on every stream frame (the SDK's
/// `:event-type` header, the `"type"` tag on busbar's decoded JSON). The set is closed; exception
/// frames carry their own type names, handled separately.
const ET_MESSAGE_START: &str = "messageStart";
const ET_CONTENT_BLOCK_START: &str = "contentBlockStart";
const ET_CONTENT_BLOCK_DELTA: &str = "contentBlockDelta";
const ET_CONTENT_BLOCK_STOP: &str = "contentBlockStop";
const ET_MESSAGE_STOP: &str = "messageStop";
const ET_METADATA: &str = "metadata";

/// The `metadata` frame's `metrics` object and `latencyMs` field, where a native stream reports its
/// real end-to-end latency (injected by `BedrockStreamFraming::inject_streaming_metrics`).
const FIELD_METRICS: &str = "metrics";
const FIELD_LATENCY_MS: &str = "latencyMs";

/// Source-spelling hint for `top_k`. Bedrock carries it in `additionalModelRequestFields` as
/// `top_k` or `topK` (the form some model families require). The reader lifts either into the IR
/// `top_k`; a naive writer would rename a `topK` Bedrock-to-Bedrock passthrough. As with
/// `MAX_COMPLETION_TOKENS_SENTINEL` in `openai_chat`, the reader stamps this sentinel when the
/// source was `topK`; the writer re-emits `topK` (else `top_k`) and consumes the sentinel. `extra`
/// is cleared across protocols, which emit the canonical `top_k`.
const TOP_K_CAMEL_SENTINEL: &str = "__busbar_top_k_camel";

/// The Converse request content grammar (`codec::drops`): a union keyed by its kind member. A block
/// of any other kind (an inline `guardContent` marker) does not cross a translate attempt.
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

/// The Converse answer content grammar.
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

/// The IR request members the reader carries by code from a path no map-file row names (how a drop
/// of one is named by the caller's wire path).
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
    // Positional stashes for a same-dialect write: cache points and documents cross as the IR's
    // cache breakpoints and media; `REQUEST_BLOCKS` names the guard markers; `topK` is a hint.
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
    // The members of each config object the reader's own code carries; the map file maps the rest
    // of what crosses.
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

/// What this dialect's answers carry beyond its map file's rows. The per-TTL cache split is read
/// into the usage detail; a stream exception is the terminal error.
const RESPONSE_CODE: &[&str] = &["usage.cacheDetails"];
const STREAM_CODE: &[&str] = &[
    INTERNAL_SERVER_EXCEPTION,
    MODEL_STREAM_ERROR_EXCEPTION,
    SERVICE_UNAVAILABLE_EXCEPTION,
    THROTTLING_EXCEPTION,
    VALIDATION_EXCEPTION,
];

/// Answer paths inside a subtree this dialect carries that its code does not, named by the drop
/// walk (paths a coarse map row covers).
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

/// Read a `reasoningContent` block into an IR `Thinking`, or `None` when it carries neither known
/// member (a future union member is left undecoded rather than mis-mapped). Its two members:
///   - `reasoningText` (`{ "text", "signature" }`) → `Thinking { text, signature }`; the opaque
///     `signature` is preserved verbatim, as Bedrock requires it echoed on a follow-up turn.
///   - `redactedContent` (opaque base64 bytes) → `Thinking { text: <bytes>, redacted: true }` so
///     the writer re-emits `redactedContent` rather than leaking bytes as plaintext; non-Bedrock
///     writers drop the redacted block.
fn read_bedrock_reasoning_block(
    reasoning: &serde_json::Value,
) -> Option<crate::codec::ir::IrBlock> {
    if let Some(reasoning_text) = reasoning.get(REASONING_TEXT) {
        let text = reasoning_text
            .get(keys::TEXT)
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        // A busbar provenance envelope (another family's signature handed to this client earlier)
        // restores the original bytes and origin; a genuine Converse signature's origin is left
        // for the caller to derive from the model id.
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

/// Build a `{"reasoningContent": ...}` block from an IR `Thinking`; the inverse of
/// `read_bedrock_reasoning_block`. A redacted `Thinking` re-emits `redactedContent` (bytes in
/// `text`); any other re-emits `reasoningText`, with `signature` only when present (Bedrock omits
/// it for an unsigned block rather than emitting `null`). Used by `write_request` and
/// `write_response`.
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

/// Build a Converse `image` block body (`{ "format", "source": { … } }`) from an `IrImageSource`, or
/// `None` when it cannot be represented. The block has only `source.bytes` (base64) and
/// `source.s3Location`, and no arbitrary-URL source:
///   - `Base64 { media_type, data }` → `source.bytes`, the subtype mapped onto the `ImageFormat`
///     union {png, jpeg, gif, webp} (jpg is jpeg). Any other subtype is dropped on the one drop
///     path, never relabelled as another format.
///   - `Vendor { vendor: "bedrock", value }` → `source.s3Location` re-emitted faithfully
///     (`uri`/`bucketOwner` preserved for a lossless same-protocol round-trip).
///   - `Url(_)` and a foreign `Vendor` (e.g. a Responses `file_id`) have no projection: dropped
///     with a warn rather than emitting a corrupt `bytes` block.
fn bedrock_image_block(source: &crate::codec::ir::IrImageSource) -> Option<serde_json::Value> {
    match source {
        // A Bedrock vendor reference is an `s3Location` (`{format, s3Location}`), re-emitted
        // faithfully; another protocol's has no projection. The caller's `format` is carried as
        // sent and none is invented.
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
        // No arbitrary-URL image source, and a foreign vendor reference has no projection; either as
        // base64 `bytes` would corrupt the block. Drop with a warn.
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
            // The subtype maps onto the `ImageFormat` union {png, jpeg, gif, webp} (`image/jpg` and
            // casing variants are `jpeg`); any other subtype, or a malformed media type, drops the block.
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

/// This dialect's name, also the `vendor` tag on an [`crate::codec::ir::IrImageSource::Vendor`]
/// this protocol produces: an `s3Location` source naming an object in the caller's AWS account,
/// meaningless to any other backend.
const VENDOR_NAME: &str = "bedrock";

/// A Converse `searchResult` block (`{source, title, content: [{text}], citations: {enabled}}`)
/// into the search-result slot (`IrBlock::search_result`), shared with Anthropic's
/// `search_result`. Text parts join with `\n` in wire order; the `citations` switch rides verbatim.
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

/// The search-result slot to a `{"searchResult": {...}}` block (the inverse of
/// [`read_bedrock_search_result`]): one text part, the `citations` switch verbatim.
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
/// Both are `{"format": "pdf", "name": "…", "source": {…}}` with a `source` of inline `bytes` or
/// an `s3Location`. `format` is a bare container token, normalized to a real mime type (the
/// neutral IR speaks mime); the writer reverses it exactly.
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
        // A text document (`source.text`, or chunked `source.content[].text`) is carried by every
        // dialect: it becomes a base64-encoded text document (the IR's `Base64.data` contract), not
        // a Vendor reference no foreign writer can re-emit.
        None if bedrock_document_text(source).is_some() => {
            crate::codec::ir::IrImageSource::Base64 {
                media_type: bedrock_text_media_type(format),
                data: busbar_contract::media::base64_encode(
                    bedrock_document_text(source).unwrap_or_default().as_bytes(),
                ),
            }
        }
        // An `s3Location` names an object in the caller's own AWS account: no other backend can
        // fetch it, so it rides the opaque `Vendor` escape and only this writer re-emits it.
        None => crate::codec::ir::IrImageSource::Vendor {
            vendor: VENDOR_NAME,
            value: source.cloned().unwrap_or_else(|| serde_json::json!({})),
        },
    };
    // A `DocumentBlock` carries the same two controls as Anthropic's: `citations: {enabled}` and a
    // free-text `context`. `VideoBlock` has neither, so they are read off a document only.
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

/// The inline text of a `DocumentSource` carrying text rather than bytes: `text`, or `content`
/// (`{text}` chunks joined by newlines). `None` for a bytes/s3 source.
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

/// The mime type of a text document source: the format's own text type (`md`, `csv`, `html`,
/// `txt`), else `text/plain`.
fn bedrock_text_media_type(format: &str) -> String {
    let m = bedrock_media_type_for_format(crate::codec::ir::IrMediaKind::Document, format);
    if m.starts_with("text/") {
        m
    } else {
        keys::TEXT_PLAIN.to_string()
    }
}

/// Converse's bare container token (`pdf`, `csv`, `mp4`) to a real mime type. An unknown token
/// falls back to the kind's generic type, not a fabricated `application/<token>`.
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

/// Project an [`crate::codec::ir::IrBlock::Media`] into the Converse block that carries it, or
/// `None` when there is no slot (the caller emits nothing, having warned). Converse has `document`
/// and `video` blocks and no audio block, each with a closed format union AWS validates, so an
/// unmappable format is dropped here rather than sent upstream to be rejected.
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
    // Converse requires `name` on a document block; a cross-protocol attachment often has none
    // (Gemini `inlineData`), so synthesize one rather than emit a block AWS rejects.
    if kind == crate::codec::ir::IrMediaKind::Document {
        block.insert(
            keys::NAME.to_string(),
            serde_json::json!(name.unwrap_or("attachment")),
        );
    } else if let Some(n) = name {
        block.insert(keys::NAME.to_string(), serde_json::json!(n));
    }
    block.insert(keys::SOURCE.to_string(), wire_source);
    // The document's citation switch and context ride `DocumentBlock` natively; a `VideoBlock` has
    // no such members, so on a video they are dropped (with a warn when set).
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

/// Mime type to a member of the closed `DocumentFormat` union, or `None` (drop with a warn).
fn bedrock_document_format(source: &crate::codec::ir::IrImageSource) -> Option<&'static str> {
    // An s3Location source carries no mime but a format is required; `pdf` is the common case.
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

/// Mime type to a member of the closed `VideoFormat` union, or `None` (drop with a warn).
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

/// Build a prompt-cache marker block (`{"cachePoint": {"type": "default"}}`). Everything before it
/// in the same `system` / `content` / `toolConfig.tools` array is cached as a prefix. The IR carries
/// the boundary as `cache_control` on a block or tool; the writer emits this marker immediately
/// after the block it applies to, where Bedrock expects it.
fn bedrock_cache_point() -> serde_json::Value {
    serde_json::json!({ (CACHE_POINT): { (keys::TYPE): "default" } })
}

/// Read `outputConfig.textFormat` (`{type: "json_schema", structure: {jsonSchema: {schema: "<JSON
/// string>", name, description}}}`) into the typed [`crate::codec::ir::IrResponseFormat`]. The
/// schema travels as a string; an unparseable one yields no directive rather than a guess.
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

/// Project [`crate::codec::ir::IrResponseFormat`] into `outputConfig.textFormat`, or `None`:
/// `OutputFormat.type` is `json_schema` only, so schema-less JSON and plain-text modes have no form.
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

/// Maps a native `cachePoint` adjacency back onto the preceding block's IR `cache_control`, so the
/// prompt-cache boundary survives a cross-protocol hop (the positional `CACHE_POINTS_SENTINEL`
/// stash is dropped there). Only kinds carrying `cache_control` (all but `Json`) can hold it; a
/// `cachePoint` after a `Json` block is left to the stash. Setting the field is additive: the
/// same-protocol stash stays, and the writer suppresses inline emission whenever it is present
/// (see `write_request`), so same-protocol round-trips stay byte-identical.
fn set_preceding_block_cache_control(blocks: &mut [crate::codec::ir::IrBlock]) {
    if let Some(last) = blocks.last_mut() {
        let cc = Some(crate::codec::ir::CacheControl {
            kind: crate::codec::ir::CacheKind::Ephemeral,
        });
        match last {
            // Thinking / Image / Media carry `cache_control` too, so a cachePoint after one lands on
            // that block as it does on Text.
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

/// Splice captured `cachePoint` blocks back into a freshly written content array at the original
/// absolute positions the reader recorded, rebuilding the native ordering on a same-protocol
/// passthrough. `entries` are the stash records (`{ "i": <usize>, "block": <value> }`) from
/// `CACHE_POINTS_SENTINEL`; one missing `i`/`block` is skipped rather than mis-placed.
///
/// Records apply in ascending index order: the reader recorded indices against the original array
/// (which contained the cachePoints), so ascending insertion reproduces its layout exactly. A
/// bounds-clamped `min(len)` puts a stale or foreign index at the end instead of panicking on the
/// request path.
fn splice_cache_points(arr: &mut Vec<serde_json::Value>, entries: &[serde_json::Value]) {
    // Collect (index, block) pairs, then sort by index so ascending insertion preserves layout.
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
/// stashes for one array) for a single `splice_cache_points` pass. Both recorded indices against
/// the same original array, so they must splice together (the helper sorts the batch): separate
/// passes would shift the second's indices. Either input may be `None`/empty.
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

/// The SigV4 region of an upstream `host` ([`EgressScheme::SigV4`]'s `region_of_host`):
/// [`derive_sigv4_region`], warning when a host names no region and the scope falls back to
/// `us-east-1`. Read once per signature.
fn declared_sigv4_region(host: &str) -> Option<&str> {
    let region = derive_sigv4_region(host);
    if region.is_none() {
        tracing::warn!(host = %host, "could not derive AWS region from Bedrock endpoint host; defaulting SigV4 scope to us-east-1 (set a bedrock-runtime[-fips].<region>.amazonaws.com host)");
    }
    region
}

/// Derive the SigV4 region from a Bedrock endpoint host. A naive `strip_prefix("bedrock-runtime.")`
/// mis-handles non-vanilla endpoints and signs for the wrong region, a 403 `SignatureDoesNotMatch`
/// the operator cannot tell from a credential error. So the known service labels (with or without
/// `-fips`) and any `vpce` front are matched, and the label after the service label is the region:
///
///   - `bedrock-runtime.<region>.amazonaws.com`
///   - `bedrock-runtime-fips.<region>.amazonaws.com`
///   - `bedrock-runtime.<region>.vpce.amazonaws.com`
///   - `vpce-0abc...-1xyz.bedrock-runtime.<region>.vpce.amazonaws.com` (interface-endpoint front)
///   - `bedrock.<region>.amazonaws.com` (the control-plane label, defensively)
///
/// `Some(region)` only when a service label is found and the next label looks like a region token
/// (alphabetic dash-parts then a numeric part: `us-east-1`, `us-gov-west-1`, `us-iso-east-1`);
/// otherwise `None`, on which the caller warns and falls back to `us-east-1`. Pure string parsing,
/// no panic.
pub(crate) fn derive_sigv4_region(host: &str) -> Option<&str> {
    // An AWS region token: alphabetic dash-parts then a final numeric part, 3-part canonical
    // (us-east-1) or 4-part partitions (us-gov-west-1, us-isob-east-1). Any dash token of >= 3
    // parts with alphabetic leading parts and an all-digit final part is accepted, so a partition
    // with more segments is not rejected (requiring exactly 3 gave GovCloud and ISO a wrong
    // `us-east-1` scope and a 403). A bare label, 2-part token, IP octet or CNAME fails the shape.
    fn looks_like_region(label: &str) -> bool {
        let parts: Vec<&str> = label.split('-').collect();
        // Need at least <area>-<direction>-<number>; no empty parts (rejects leading/trailing/
        // doubled dashes).
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

    // Walk the dotted labels; when we hit a Bedrock service label, the NEXT label is the region.
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
/// `ImageSource` has `source.bytes` (base64) and `source.s3Location` (`{"uri","bucketOwner"}`).
/// An s3Location (plus the captured `format`) rides `IrImageSource::Vendor { vendor: "bedrock",
/// value }` so `bedrock_image_block` re-emits it same-protocol (a foreign writer drops it);
/// reading only `bytes` would silently drop an S3 image. Base64 reads as `IrImageSource::Base64`.
/// A block with neither yields `None`, not an empty-bytes block.
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

    // Otherwise an `s3Location` source, carried as `{format, s3Location}` so the writer re-emits it
    // faithfully same-protocol.
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

/// Model a `guardContent` block's content: `{text: {text, qualifiers}}` is prompt text and
/// `{image: {format, source}}` a prompt image. The qualifiers (which spans a guardrail evaluates)
/// have no neutral form and ride only the positional stash. `None` if neither member is present.
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
/// `guardContent` / `document` / `video` record), for one message (`Some(m)`) or the `system` array
/// (`None`). The writer suppresses its own emission of these because the verbatim splice re-emits
/// them, keeping a same-protocol round-trip byte-identical while a cross-protocol IR (stash
/// cleared) carries the modelled block.
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

/// Normalize `toolConfig.toolChoice` into the IR union: `{"auto":{}}` is `Auto`, `{"any":{}}` is
/// `Required`, `{"tool":{"name":"X"}}` is `Tool{name:"X"}`. There is no native "none". An absent
/// or unrecognized shape yields `None`, so no spurious directive appears. Takes the whole
/// `toolConfig` object.
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

/// Emit the IR tool-choice union as Bedrock's `toolChoice`. `None` for `IrToolChoice::None`: there
/// is no native "don't call a tool", so `toolChoice` is omitted rather than an invalid shape sent.
fn write_bedrock_tool_choice(tc: &crate::codec::ir::IrToolChoice) -> Option<serde_json::Value> {
    match tc {
        crate::codec::ir::IrToolChoice::Auto => Some(serde_json::json!({(keys::AUTO): {}})),
        crate::codec::ir::IrToolChoice::Required => Some(serde_json::json!({(keys::ANY): {}})),
        crate::codec::ir::IrToolChoice::Tool { name } => {
            // `toolChoice.tool` (force a specific tool) is documented as Claude-only on Converse;
            // other families reject it. The writer cannot gate on the model (it receives only the
            // IR, and Bedrock carries the model in the URL), so it emits it and warns, making a
            // non-Claude target's ValidationException diagnosable.
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

/// Bedrock stopReason → canonical IR stop_reason.
fn stop_reason_map(ward: &str) -> crate::codec::ir::IrStopReason {
    use crate::codec::ir::IrStopReason as S;
    match ward {
        keys::END_TURN => S::EndTurn,
        keys::TOOL_USE => S::ToolUse,
        keys::MAX_TOKENS => S::MaxTokens,
        keys::STOP_SEQUENCE => S::StopSequence,
        // Both moderation outcomes fold to the canonical `Safety`.
        CONTENT_FILTERED | "guardrail_intervened" => S::Safety,
        // Generation ended because the context window filled: a token-limit cutoff, `MaxTokens`
        // to every client dialect.
        STOP_MODEL_CONTEXT_WINDOW_EXCEEDED => S::MaxTokens,
        // Output Bedrock could not parse reads as `Other`: the answer is served and billed.
        // `Error` would turn a buffered answer into a billed 502 and a lane fault.
        _ => S::Other,
    }
}

/// Whether a model id names a Claude model (`anthropic.` / `claude`, covering bare ids, inference
/// profiles and ARNs). The body carries no model family, so the id is the only signal
/// (see `bedrock_signature_origin`).
fn bedrock_model_is_claude(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("anthropic.") || m.contains("claude")
}

/// Which family minted the reasoning signatures of a conversation addressed to `model`. Claude is
/// `Anthropic` (it mints Anthropic signatures). A model id visibly naming another provider
/// (`<provider>.<model>`, optionally behind a geographic profile prefix or ARN) is `BedrockOther`.
/// Anything the id does not reveal (no model, a busbar alias, an opaque application-profile ARN)
/// is `None`, never a guess.
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

/// Converse `requestMetadata` (string to string, filters the caller's invocation logs) to the typed
/// [`crate::codec::ir::IrRequest::metadata`]; a non-string value is skipped. The raw object still
/// rides `extra`, so a same-protocol hop re-emits it verbatim.
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

/// Whether `s` fits a `requestMetadata` key/value: `min..=256` characters in
/// `[a-zA-Z0-9\s:_@$#=/+,-.]`. OpenAI allows 512-character values of any character, so a
/// cross-protocol entry can fall outside it.
fn bedrock_request_metadata_fits(s: &str, min: usize) -> bool {
    let n = s.chars().count();
    (min..=256).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || ":_@$#=/+,-.".contains(c))
}

/// The typed [`crate::codec::ir::IrRequest::metadata`] to `requestMetadata`, same keys. An entry
/// Converse would reject (the pattern, length, or the 16-entry cap) is dropped with a warn and the
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

/// The refinement a stopReason states beyond its coarse reason: `model_context_window_exceeded` is
/// `MaxTokens` plus [`crate::codec::ir::IrStopDetail::ContextWindowExceeded`], so a dialect that
/// names it (Anthropic) receives it. Every other stopReason states no detail.
fn stop_detail_map(ward: &str) -> Option<crate::codec::ir::IrStopDetail> {
    (ward == STOP_MODEL_CONTEXT_WINDOW_EXCEEDED)
        .then_some(crate::codec::ir::IrStopDetail::ContextWindowExceeded)
}

/// Converse's `model_context_window_exceeded` stopReason.
const STOP_MODEL_CONTEXT_WINDOW_EXCEEDED: &str = "model_context_window_exceeded";

/// IR stop_reason (plus its [`crate::codec::ir::IrStopDetail`]) to stopReason. A context-window
/// detail is written as `model_context_window_exceeded`; any other detail has no spelling here, so
/// the coarse reason is written.
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

/// IR stop_reason to stopReason (inverse of `stop_reason_map`).
fn stop_reason_reverse(canonical: crate::codec::ir::IrStopReason) -> &'static str {
    use crate::codec::ir::IrStopReason as S;
    match canonical {
        S::EndTurn => keys::END_TURN,
        S::ToolUse => keys::TOOL_USE,
        S::MaxTokens => keys::MAX_TOKENS,
        S::StopSequence => keys::STOP_SEQUENCE,
        S::Safety => CONTENT_FILTERED,
        // A refusal is a content-policy stop, `content_filtered` in the closed enum (the refusal
        // text still rides the content).
        S::Refusal => CONTENT_FILTERED,
        // error / pause_turn / other → end_turn rather than an off-spec value a strict Converse
        // client rejects.
        S::Error | S::PauseTurn | S::Other => keys::END_TURN,
    }
}

/// Bedrock's usage counts, as data. The cache write is `cacheWriteInputTokens` (a creation in
/// Anthropic terms) and the read `cacheReadInputTokens`; both are optional, so each is `None` when
/// absent (distinct from `Some(0)`). `cacheDetails` is the per-TTL breakdown of the write (a list
/// of `{ttl, inputTokens}`, 1h before 5m); the TTLs are separate tiers, each the sum of its
/// entries (`CountRead::ListSum`). An unreported TTL stays `None`, and an unrecognized `ttl`
/// reaches no tier (the `cacheWriteInputTokens` total still carries it). The buffered response,
/// the stream's `metadata` frame and a truncated-body recovery read this one table.
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

/// Stable identifier of the identity [`read_bedrock_usage`] checks `usage.totalTokens` against,
/// carried on [`crate::codec::ir::UsageIdentityNote::identity`].
const BEDROCK_USAGE_IDENTITY: &str = "bedrock.usage";

/// A Converse `usage` object (`None` when absent) to the IR usage, through [`USAGE`].
///
/// Every count the pinned wire lock (`testing/llm-conformance/wire/bedrock.wire.json`) declares
/// under `usage` is ledgered or a slice of a ledgered count: `inputTokens`, `outputTokens`,
/// `cacheReadInputTokens`, `cacheWriteInputTokens`; `cacheDetails[].inputTokens` is the per-TTL
/// split of the cache write, carried as 5m/1h attribution inside the one cache-write class.
/// `totalTokens` is AWS's sum of the four (see the writer's `converse_total_tokens`), never a
/// unit: it is cross-checked, a gap is WARN-logged and carried as the usage identity note, and
/// the gap above the four is the open class `unitemized_tokens` (`IrUsage::to_token_usage`).
/// Guardrail policy units ride `trace`, not `usage`: see [`guardrail_units`].
fn read_bedrock_usage(
    usage_obj: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let mut ir = crate::codec::usage_count::read_usage(VENDOR_NAME, usage_obj, USAGE)?;
    // A refused count leaves no gap to ledger (`refused_any`): the total is not cross-checked.
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

/// The members of a Converse `TokenUsage` object: a `usage` object naming none of them is not the
/// turn's token usage (a guardrail's `invocationMetrics.usage` names policy units instead).
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
/// A response (or the stream's `metadata` frame) that ran a guardrail carries
/// `trace.guardrail.inputAssessment.<id>.invocationMetrics.usage` and
/// `trace.guardrail.outputAssessments.<id>[].invocationMetrics.usage`: policy units AWS bills
/// separately from tokens, per policy type. Each is its own open class
/// ([`crate::codec::ir::open_class::GUARDRAIL_COUNT_CLASSES`]), never folded into a token class;
/// every reported unit is a ledger line priced by the ratecard. Counts sum over every guardrail
/// and assessment on both sides; a zero sum is no hit.
///
/// A count that is present and not a count is refused on its own: it bills nothing and is warned,
/// never read as zero, while the readable counts beside it bill. `null` is absence.
///
/// The assessment's other integers (`guardrailProcessingLatency`, `guardrailCoverage.*`) are
/// latency and the coverage the units are computed from, not billed; ledgering coverage would
/// count the units twice.
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
            // Each count is judged on its own: an unreadable one is
            // refused loudly and bills nothing; the readable ones bill as stated.
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

/// The tier that served an answer (`serviceTier.type`) in the IR's words (`IrUsageDetail::service_tier`).
fn read_served_tier(answer: &serde_json::Value) -> Option<String> {
    crate::codec::carry::read_word(
        map::WORDS_SERVICE_TIER,
        answer
            .get(SERVICE_TIER_CAMEL)
            .and_then(|t| t.get(keys::TYPE)),
    )
}

/// The `serviceTier` member a Converse answer carries for `usage`'s served tier, when the dialect
/// has a word for it.
fn served_tier_member(usage: &crate::codec::ir::IrUsage) -> Option<serde_json::Value> {
    let tier = usage.detail.service_tier.as_deref()?;
    let word = crate::codec::carry::word_out(map::WORDS_SERVICE_TIER, tier)?;
    Some(serde_json::json!({ (keys::TYPE): word }))
}

/// The `CacheTTL` enum's two values, as the Bedrock service model spells them.
const CACHE_TTL_5M: &str = "5m";
const CACHE_TTL_1H: &str = "1h";

/// Write the per-TTL cache-write split onto a `usage` object, the inverse of the `cacheDetails`
/// rows in [`USAGE`]: entries in documented order (1h before 5m), only for tiers the IR carries
/// (`cacheDetails` is empty if no cache creation occurred, so no split means no member).
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

/// Write the prompt-cache token fields onto a `usage` object, the inverse of the cache rows in
/// [`USAGE`]: `cacheWriteInputTokens` and `cacheReadInputTokens`, only when the IR carries a value.
/// `None` is omitted, not `0`, so a Bedrock-to-Bedrock round-trip of a no-cache response matches
/// native AWS (which omits them) and fabricates no cache-accounting tell.
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
    // The per-TTL breakdown of the write total above: priced separately, so carried separately.
    write_cache_details(usage_obj, usage);
}

/// Upper bound on the upstream-controlled `contentBlockIndex` at every stream read site. A hostile
/// backend can send up to `u64::MAX`, and a downstream writer keying per-index state off it would
/// allocate against a pathological index; a real stream emits small sequential indices, so the
/// value is clamped before it enters the IR. Mirrors the OpenAI `MAX_TOOL_INDEX` and Cohere
/// `MAX_TOOL_FRAME_INDEX` clamps.
const MAX_CONTENT_BLOCK_INDEX: u64 = 1023;

/// Read `contentBlockIndex` off a frame, 0 when absent or non-numeric, clamped to
/// `MAX_CONTENT_BLOCK_INDEX`. Shared by all three stream read sites.
fn clamp_content_block_index(data: &serde_json::Value) -> usize {
    data.get(CONTENT_BLOCK_INDEX)
        .and_then(|i| i.as_u64())
        .unwrap_or(0)
        .min(MAX_CONTENT_BLOCK_INDEX) as usize
}

#[derive(Clone)]
pub struct BedrockReader;

/// Bedrock-ingress per-stream framing. A native ConverseStream ends with a `messageStop` frame
/// followed by exactly one `metadata` (usage) frame, but the IR carries one combined
/// `MessageDelta{stop_reason, usage}` and a foreign backend can split stop and usage across two
/// events. This fans the combined delta into a stop-only delta (`messageStop`) plus, when usage
/// rode with the stop, a usage-only delta (`metadata`); otherwise it defers the metadata to a
/// trailing usage-only delta (OpenAI `include_usage`) or, if none arrives, to the finish-time flush.
/// The `emitted`/`pending` flags enforce the one-metadata invariant. Built per stream via
/// [`BedrockWriter::new_stream_framing`]; reached only on Bedrock ingress.
#[derive(Default)]
struct BedrockStreamFraming {
    /// Whether a `metadata` frame was already emitted; guards the exactly-one invariant (suppresses
    /// a duplicate usage-only delta and the finish flush).
    emitted: bool,
    /// Set when a stop-delta arrived with all-zero usage and the `metadata` frame was deferred
    /// awaiting a trailing usage-only delta. If it never arrives (default OpenAI streaming),
    /// `on_finish` flushes a single zero-usage `metadata` so the stream always has its terminal frame.
    pending: bool,
}

impl super::proto_codec::StreamFraming for BedrockStreamFraming {
    // Usage rides a separate `metadata` frame (on_combined_stop_delta / on_usage_only_delta /
    // on_finish), so the generic terminal-usage deferral must not apply.
    fn folds_terminal_usage(&self) -> bool {
        false
    }

    fn abort_exception_type(&self) -> Option<&'static str> {
        // An aborted native stream emits a modeled exception frame; `InternalServerException` (the
        // generic server fault) keeps the close well-formed for the SDK decoder, and the wire type
        // name stays in this module, not the agnostic translator.
        Some(EXC_INTERNAL_SERVER)
    }

    fn inject_streaming_metrics(
        &self,
        event_type: &str,
        data: &mut serde_json::Value,
        elapsed_ms: Option<u64>,
    ) {
        // A native `metadata` frame carries a required `metrics.latencyMs`. Inject the elapsed
        // wall-clock since the first byte; a frame already carrying one (a same-protocol upstream's)
        // is left alone, and unavailable timing is emitted as `0` rather than dropped. The writer
        // leaves `metrics` off, so this is its single source. Only `metadata` is special.
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
        // Frame 1: stop-only delta → `messageStop` (usage, if any, rides frame 2).
        let mut events = vec![crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(stop_reason),
            stop_sequence: stop_sequence.clone(),
            usage: crate::codec::ir::IrUsage::default(),
            stop_detail: None,
        }];
        // Frame 2: `metadata` with the token usage; a native stream emits exactly one. Emit it only
        // if real usage rode with the stop (native Bedrock, or any egress bundling usage into the
        // stop delta). All-zero usage is an OpenAI `include_usage` stop chunk whose tokens arrive
        // in a separate trailing delta: defer to it so the metadata carries the real tokens, never
        // a zero-usage frame.
        // Guard on `!self.emitted` so a second combined stop-delta cannot produce a second
        // `metadata`, even from a hostile backend; a repeat yields only the idempotent stop frame.
        // Cache-only usage counts too: a full cache hit can have zero input and output tokens but
        // non-zero cache tokens; omitting them deferred the frame and flushed a zero-usage one,
        // under-reporting cache tokens to the client SDK.
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
                // Deferred: the stop carried no usage. A trailing `include_usage` delta emits the
                // metadata if it arrives; default OpenAI streaming never sends one, so mark it
                // pending and let `on_finish` flush a single zero-usage frame. A native stream
                // always ends with a metadata frame; its absence is a tell and loses accounting.
                self.pending = true;
            }
        }
        Some(events)
    }

    fn on_usage_only_delta(&mut self) -> Option<bool> {
        // A usage-only delta (`stop_reason: None`) becomes a `metadata` frame (the trailing OpenAI
        // `include_usage` chunk or a native usage frame), suppressed if one already rode with the stop.
        if self.emitted {
            return Some(false);
        }
        self.emitted = true;
        self.pending = false; // the deferral is now resolved
        Some(true)
    }

    fn on_finish(&mut self) -> Option<crate::codec::ir::IrStreamEvent> {
        // If a stop-delta deferred `metadata` (zero usage) and the trailing usage-only delta never
        // arrived (default OpenAI streaming), flush a single zero-usage `metadata` now.
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

/// Per-stream set of IR block indices this writer opened and so owes a `contentBlockStop` (Text /
/// ToolUse / Thinking, not `Image`, whose `BlockStart` maps to `None`). Tracked does not mean a
/// `contentBlockStart` was emitted: only `ToolUse` projects one; Text and Thinking open implicitly
/// on their first `contentBlockDelta` (`ContentBlockStart$start` models only `toolUse`). The
/// `BlockStop` arm carries only an index, so this set is how it tells an untracked index from a
/// tracked one rather than emitting an orphan stop. Mirrors `CohereWriter`'s guard.
#[derive(Clone)]
pub struct BedrockWriter {
    open_block_indices: StreamCell<std::collections::BTreeSet<usize>>,
}

/// Value-namespace constructor for [`BedrockWriter`], like `CohereWriter`'s. `protocol_for` builds
/// a fresh writer per stream, so each use inlines an independent empty set.
/// `clippy::declare_interior_mutable_const` is suppressed deliberately: a shared `static` would
/// leak one stream's open indices into another.
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
pub const BedrockWriter: BedrockWriter = BedrockWriter {
    open_block_indices: StreamCell::new(std::collections::BTreeSet::new()),
};

impl BedrockWriter {
    /// Record that IR block `index` was opened and owes a `contentBlockStop` (whether or not a start
    /// was emitted). Lock poisoning degrades to a no-op rather than panicking.
    fn mark_block_open(&self, index: usize) {
        self.open_block_indices.lock().insert(index);
    }

    /// True, forgetting `index`, if the block was opened (its `BlockStop` emits a
    /// `contentBlockStop`); false if never opened (e.g. `Image`), so the `BlockStop` emits nothing.
    /// Lock poisoning degrades to `false` rather than panicking.
    fn take_block_open(&self, index: usize) -> bool {
        self.open_block_indices.lock().remove(&index)
    }
}

/// Wrap a single non-stream `IrResponse` into a ConverseStream `application/vnd.amazon.eventstream`
/// byte sequence, for a bedrock-ingress client that requested `ConverseStream` (`wants_stream`)
/// when the cross-protocol upstream answered with a buffered 2xx. A plain JSON Converse body is
/// undecodable by the SDK's eventstream decoder, a hard failure and a tell. This synthesizes the
/// native sequence: one `messageStart`, per content block a `contentBlockStart` + delta(s) +
/// `contentBlockStop`, then `messageStop` (stop reason) and a trailing `metadata` (token usage),
/// the two-frame split the writer's `MessageDelta` arm expects. Each event goes through the same
/// writer as the live path and is encoded via `eventstream::encode_frame`. Never panics: a frame
/// whose payload fails to serialize is skipped.
pub fn bedrock_response_to_eventstream(
    host: &dyn busbar_contract::codec::CodecHost,
    ir: &crate::codec::ir::IrResponse,
    elapsed_ms: Option<u64>,
) -> Vec<u8> {
    use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStreamEvent, IrUsage};
    let writer = protocol();
    let writer = writer.writer();
    let mut out: Vec<u8> = Vec::new();
    // Render one IR event through the writer and append the frame, if it maps to one. Uses
    // `write_response_events`, so a multi-citation delta frames as one `citation` delta each.
    let push = |ev: &IrStreamEvent, out: &mut Vec<u8>| {
        for (event_type, mut payload) in writer.write_response_events(host, ev) {
            // A native `metadata` frame always carries `metrics.latencyMs`. The writer's
            // `MessageDelta` arm omits it and the live StreamTranslate path injects it; this buffered
            // path bypasses StreamTranslate, so inject it here (a missing required member is a
            // tell), using the request's elapsed wall-clock, even when timing is unavailable
            // (see `ensure_metrics`).
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

    // Per block: contentBlockStart, contentBlockDelta(s), contentBlockStop, as the live fan-out
    // (`read_response_events`). `index` advances only when a block emits frames, not via
    // `enumerate()`: ToolResult/Image/Json emit nothing and would leave a gap a native stream never has.
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
                // The block's citations ride the same `contentBlockIndex` as `citation` deltas, as on
                // the live path, so a synthesized stream carries the buffered body's sources.
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
            // A Thinking block streams as `contentBlockDelta.reasoningContent` frames closed by a
            // `contentBlockStop`, with no `contentBlockStart` (`ContentBlockStart$start` models only
            // `toolUse`). The writer maps its start to None (marking it open so the stop fires) and
            // re-emits each Thinking/Signature/RedactedReasoning delta. Same sequence as the live path.
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
                    // Opaque encrypted reasoning: `text` holds the bytes, one delta carries them.
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
            // ToolResult/Image/Json have no content-delta frame here; skipped without advancing
            // `index`. Enumerated explicitly so a new `IrBlock` variant fails to compile.
            IrBlock::ToolResult { .. }
            | IrBlock::Image { .. }
            | IrBlock::Media { .. }
            | IrBlock::Json(_)
            | IrBlock::HostedToolRecord { .. } => {}
        }
    }

    // messageStop (stop reason) then metadata (usage): the writer's `MessageDelta` arm maps a
    // stop-bearing delta to `messageStop` and a usage-only delta to `metadata`.
    // Default the stop reason from the IR content, not unconditionally `end_turn`: Converse reports
    // `tool_use` for a turn that emitted a tool call, so a buffered IR with a ToolUse block and no
    // stop_reason defaults to `tool_use`, or an SDK consumer keying agentic control flow off
    // stopReason would not re-invoke the tool. `end_turn` only when there was no tool call.
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
            // The refinement rides the stop-bearing frame, so a buffered context-window stop
            // synthesizes the same `messageStop` a streamed one does.
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

/// Build a `metrics` object carrying `latencyMs`, the one member the Converse `metrics` shape has.
fn metrics_object(latency_ms: u64) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    metrics.insert(
        FIELD_LATENCY_MS.to_string(),
        serde_json::Value::from(latency_ms),
    );
    serde_json::Value::Object(metrics)
}

/// True when `value` already carries a well-formed `metrics.latencyMs` (an integer), i.e. the
/// upstream's own measurement, which is always passed through in preference to busbar's.
fn has_valid_metrics(value: &serde_json::Value) -> bool {
    value
        .get(FIELD_METRICS)
        .and_then(|m| m.get(FIELD_LATENCY_MS))
        .is_some_and(|ms| ms.is_u64() || ms.is_i64())
}

/// Ensure a Converse-shaped object carries `metrics.latencyMs`: an upstream `metrics` is kept,
/// else busbar's measured latency is written. `metrics` is required (also on the `metadata` event),
/// so with no measurement it is emitted as `0` rather than dropped. Non-objects are left alone.
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

/// IR mapping — the typed IR slots and the lane capabilities (BED-10, IR-03/10/11/12/18,
/// IR-09, LaneCaps).
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
/// `detected` or blocked, its `type` the category. `confidence` and `filterStrength` are AWS's own
/// scale and do not cross; the other policies (topics, words, PII, grounding) have no counterpart.
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
