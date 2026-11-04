// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! LLM dialect declarations and computed codec facade.

use crate::codec::cells::CodecHost;
use busbar_contract::operation::OpVerb;
pub use busbar_contract::protocol::{
    find_frame_terminator, head_field, sse_line_spans, sse_lines, HeadFields, IrError, SseFrames,
    APPLICATION_JSON, DISPOSITION_CONTEXT_LENGTH, EGRESS_UA_DEFAULT, ERR_TYPE_API_ERROR,
    ERR_TYPE_AUTHENTICATION, ERR_TYPE_INSUFFICIENT_QUOTA, ERR_TYPE_INVALID_REQUEST,
    ERR_TYPE_NOT_FOUND, ERR_TYPE_OVERLOADED, ERR_TYPE_PERMISSION, ERR_TYPE_RATE_LIMIT,
    ERR_TYPE_REQUEST_TOO_LARGE, ERR_TYPE_SERVER_ERROR, KIND_API_ERROR, KIND_AUTHENTICATION,
    KIND_INSUFFICIENT_QUOTA, KIND_INVALID_REQUEST, KIND_NOT_FOUND, KIND_OVERLOADED,
    KIND_PERMISSION, KIND_RATE_LIMIT, KIND_REQUEST_TOO_LARGE, KIND_SERVER_ERROR, KIND_TIMEOUT,
    PROVIDER_CODE_CONTEXT_LENGTH, SIGNAL_IR_PARSE, STREAM_ABORT_DETAIL, TEXT_EVENT_STREAM,
};
pub use busbar_contract::protocol::{
    CredentialFamily, CredentialHeader, EgressAuthHeaders, EgressScheme, IngressAuth,
    SigningContext,
};

/// How tightly a protocol CLAIMS an inbound request, for the generic detection fold. A LOWER value
/// binds TIGHTER — it names an earlier rung of the historical detection ladder (a mandatory-unique
/// credential header binds tighter than a path verb, which binds tighter than a bare path suffix).
/// The fold picks the tightest claim across the registered protocols; a tie breaks by registration
/// order. Opaque to core: only the relative order is meaningful, and each protocol owns the rungs
/// it claims. This is the datum that let a hand-ordered `if`-ladder in the kernel become a fold
/// over per-declaration predicates — each dialect's specific header/path sniff now states its own
/// rungs on its declaration's `claims`, and the kernel names no dialect.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ClaimStrength(pub u16);

/// The ROUTER detection predicate a protocol supplies: `(fields, path) -> Option<ClaimStrength>`,
pub type ClaimsFn = fn(HeadFields<'_>, &str) -> Option<ClaimStrength>;

/// The RESIDUAL detection predicate a protocol supplies: `path -> Option<ClaimStrength>`, from the
pub type ResidualClaimsFn = fn(&str) -> Option<ClaimStrength>;

/// A protocol's RESPONSE-side vendor-metadata reporter: given a response body, the vendor-scoped
pub type VendorResponseMetadataFn = fn(&serde_json::Value) -> Vec<&'static str>;

/// A streaming JSON-array reframer: consumes a protocol's event-stream response bytes and re-emits
pub trait ArrayStreamFramer: Send {
    /// Feed a chunk of event-stream bytes; return JSON-array bytes for whatever complete frames are
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8>;

    /// Close the array at end-of-stream when this framer sits DOWNSTREAM of a cross-protocol
    fn finish_for_translate(&mut self, translate_aborted: bool) -> Vec<u8>;

    /// Terminate the array with a trailing protocol-shaped SERVER-ERROR element, then the closing `]`.
    fn finish_with_server_error(&mut self, message: &str) -> Vec<u8>;
}

/// **THE 4TH NEUTRAL SEAM.** The per-PROTOCOL computed-codec facade
pub trait DialectCodec: Send + Sync {
    fn probe_body(&self, host: &dyn CodecHost, model: &str) -> Vec<u8>;
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> bool;
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage>;
    fn ingress_response_request_id(
        &self,
        host: &dyn CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)>;
    fn write_error(
        &self,
        host: &dyn CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value;
    fn requested_candidate_count(&self, body: &serde_json::Value) -> Option<u64>;
    fn write_response_exception(
        &self,
        host: &dyn CodecHost,
        err: &busbar_contract::upstream::CanonicalSignal,
    ) -> Option<(String, String)>;
    fn write_error_frame(
        &self,
        host: &dyn CodecHost,
        err: &busbar_contract::upstream::CanonicalSignal,
    ) -> Option<(String, serde_json::Value)>;
    fn wants_array_stream(&self, body: &serde_json::Value) -> bool;
    fn inject_response_metrics(&self, value: &mut serde_json::Value, elapsed_ms: Option<u64>);
    /// The head fields this dialect's error response carries beside its body, given the built
    fn error_response_fields(
        &self,
        host: &dyn CodecHost,
        kind: &str,
        envelope: &serde_json::Value,
    ) -> Vec<(String, Vec<u8>)>;
    /// This protocol's upstream-error vocabulary (the reader's `extract_error`), reached by name so
    fn extract_error(
        &self,
        status: u16,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError;
    /// The dialect's array-stream framer for a JSON-array ingress client, or `None` when this
    fn make_array_stream_framer(&self) -> Option<Box<dyn ArrayStreamFramer>>;
    /// The upstream request path for a (streaming) request against this dialect — the health probe's
    fn upstream_path_for_stream(&self, model: &str, stream: bool) -> String;
    /// Install the authoritative lane model into a same-protocol passthrough body if the dialect
    fn rewrite_model_if_needed(&self, body: &mut serde_json::Value, model: &str) -> bool;
    /// Reshape a path-base (URL-model) lane's body for this dialect (e.g. drop the body `model`,
    fn reshape_for_path_base(&self, body: &mut serde_json::Value) -> bool;
}

/// EVERYTHING CORE KNOWS ABOUT A PROTOCOL, declared once by the protocol itself.
pub struct ProtocolDecl {
    /// The registry key, and the metrics label. **OPERATOR-VISIBLE:** a protocol name appears in
    pub name: &'static str,

    /// This protocol's NEUTRAL computed-codec facade ([`DialectCodec`]), or `None` for a protocol
    pub codec: Option<&'static dyn DialectCodec>,

    /// The cell that serves one exchange on this protocol. Replaces `handlers::request_handler`'s
    pub handler: Option<&'static dyn crate::codec::cells::RequestHandler>,

    /// THE VERBS this protocol serves — one [`OpVerb`] (`Verb { op, name }`
    pub verbs: &'static [OpVerb],

    /// TOP-LEVEL body keys the pre-materialized path may point-read, DOM-free. The registry unions
    pub head_keys: &'static [&'static str],

    /// The `Content-Type` this protocol's writer emits on a STREAMING response, or `None` for a
    pub streaming_content_type: Option<&'static str>,

    /// The router's array-stream shim key for this protocol (only one dialect has one: a marker
    pub array_stream_shim_key: Option<&'static str>,

    /// This protocol's NATIVE tool-call id prefix, or `None` when it carries no tool id on the wire
    pub native_tool_id_prefix: Option<&'static str>,

    /// Which inbound auth scheme this protocol's clients present.
    pub ingress_auth: IngressAuth,

    /// This protocol's NATIVE egress credential-header builder, or `None` for a protocol whose
    pub egress_auth_headers: Option<EgressAuthHeaders>,

    /// Whether [`Self::egress_auth_headers`]'s output is LANE-CONSTANT: a pure function of the
    pub egress_auth_lane_constant: bool,

    /// This protocol's DECLARED egress scheme, which the kernel's egress-auth unit presents the
    pub egress_scheme: Option<EgressScheme>,

    /// Whether a STREAMING response on this protocol reports token usage only when the request
    pub stream_usage_requires_opt_in: bool,

    /// Replaces `ProtocolWriter::requires_max_tokens()`. Whether this dialect hard-rejects a request
    pub requires_max_tokens: bool,

    /// Replaces `ProtocolWriter::stop_sequence_cap()`. The published cap on stop sequences and the
    pub stop_sequence_cap: Option<(usize, &'static str)>,

    /// Replaces `ProtocolWriter::cache_markers_model_gated()`. Whether this dialect's native cache
    pub cache_markers_model_gated: bool,

    /// Replaces `ProtocolWriter::fills_thought_signature()`. Whether egress fills the
    pub fills_thought_signature: bool,

    /// Replaces `ProtocolWriter::frame_after_message_start()`. A framed wire frame this dialect emits
    pub frame_after_message_start: Option<&'static [u8]>,

    /// Replaces `ProtocolWriter::reshapes_body_at_path_base()` (the PREDICATE only). Whether this
    pub reshapes_body_at_path_base: bool,

    /// Replaces `ProtocolWriter::max_cache_control_breakpoints()`. The maximum `cache_control`
    pub max_cache_control_breakpoints: Option<usize>,

    /// Replaces `ProtocolWriter::quota_exceeded_status()`. The native HTTP status a quota/budget
    pub quota_exceeded_status: u16,

    /// Replaces `ProtocolWriter::ingress_is_eventstream()`. True when this protocol's ingress client
    pub ingress_is_eventstream: bool,

    /// Replaces the writer's former done-terminator method. True when this protocol's streamed
    pub emits_sse_done_terminator: bool,

    /// Replaces `ProtocolWriter::max_citations_per_delta()`. The maximum citations one streamed
    pub max_citations_per_delta: Option<usize>,

    /// Replaces `ProtocolWriter::egress_user_agent()`. The plausible native-SDK `User-Agent` for THIS
    pub egress_user_agent: &'static str,

    /// Replaces `ProtocolWriter::has_model_in_url()`. True when this protocol carries the model in the
    pub has_model_in_url: bool,

    /// Replaces `ProtocolWriter::auth_failure_status_and_kind()`. The HTTP status and error `kind` a
    pub auth_failure_status_and_kind: (u16, &'static str),

    /// Replaces `ProtocolWriter::ingress_relays_amzn_headers()`. True when this protocol's ingress
    pub ingress_relays_amzn_headers: bool,

    /// Replaces `ProtocolWriter::ingress_relayed_response_header_names()`. The upstream response
    pub ingress_relayed_response_header_names: &'static [&'static str],

    /// Replaces `ProtocolWriter::auth_failure_message()`. The vendor-plausible auth-failure wire
    pub auth_failure_message: &'static str,

    /// Replaces `ProtocolWriter::uses_array_stream_shim()`. True when this protocol's ingress
    pub uses_array_stream_shim: bool,

    /// Replaces `ProtocolWriter::has_native_path_not_found()`. True when this protocol has a native
    pub has_native_path_not_found: bool,

    /// Replaces `ProtocolWriter::egress_accept()` (the STREAMING half of it). The native-SDK
    pub egress_stream_accept: &'static str,

    /// This protocol's `GET /v1(beta)/models` (list-models) response ENVELOPE builder, or `None`
    pub models_list_envelope: Option<fn(&[&str]) -> serde_json::Value>,

    /// THE ROUTER detection predicate — how (and how tightly) this protocol claims an inbound
    pub claims: Option<ClaimsFn>,

    /// THE RESIDUAL detection predicate — how (and how tightly) this protocol claims a path from its
    pub residual_claims: Option<ResidualClaimsFn>,

    /// TRUE for the ONE protocol core falls back to when NO dialect claims a request yet a dialect
    pub residual_default: bool,

    /// THE RESPONSE-side vendor-metadata reporter — the fields this protocol's upstream returns that
    pub vendor_response_metadata: Option<VendorResponseMetadataFn>,

    /// THE WIRE-FINGERPRINT HEADERS this dialect declares as SAFE disambiguators of the SHARED
    pub list_models_fingerprint_headers: &'static [&'static str],

    /// THE STATIC EGRESS HEADERS this protocol's upstream request always carries beside its declared
    pub static_headers: &'static [(&'static str, &'static str)],
}

impl ProtocolDecl {
    /// This dialect's router claim on `(path, fields)`: [`ProtocolDecl::claims`] over a request's
    #[must_use]
    pub fn claim_over(&self, path: &str, fields: HeadFields<'_>) -> Option<ClaimStrength> {
        (self.claims?)(fields, path)
    }

    /// A NAME-ONLY DECLARATION: this key, and the neutral zero for every other field.
    ///
    /// The declaration has forty-odd fields and almost every one of them is "no". A caller that
    /// needs a decl in order to exercise a fold over `name`/`has_model_in_url` had to write all
    /// forty out, and two copies of that literal already existed — one here, one on the crate that
    /// holds the fold. Two copies of a struct literal is two places a NEW field has to be added,
    /// and the second one is the one that gets missed.
    ///
    /// So the neutral row is written ONCE, beside the struct whose fields it names, and a caller
    /// states only what is true of its own case:
    ///
    /// ```ignore
    /// static URL_MODEL: ProtocolDecl = ProtocolDecl { has_model_in_url: true, ..ProtocolDecl::named("telex") };
    /// ```
    ///
    /// `const`, so it composes into a `static` without a lock or a lazy cell.
    pub const fn named(name: &'static str) -> Self {
        Self {
            name,
            codec: None,
            handler: None,
            verbs: &[],
            head_keys: &[],
            streaming_content_type: None,
            array_stream_shim_key: None,
            native_tool_id_prefix: None,
            ingress_auth: IngressAuth::Bearer,
            egress_auth_headers: None,
            egress_auth_lane_constant: false,
            egress_scheme: None,
            stream_usage_requires_opt_in: false,
            requires_max_tokens: false,
            stop_sequence_cap: None,
            cache_markers_model_gated: false,
            fills_thought_signature: false,
            frame_after_message_start: None,
            reshapes_body_at_path_base: false,
            max_cache_control_breakpoints: None,
            quota_exceeded_status: 429,
            ingress_is_eventstream: false,
            emits_sse_done_terminator: false,
            max_citations_per_delta: None,
            egress_user_agent: EGRESS_UA_DEFAULT,
            has_model_in_url: false,
            auth_failure_status_and_kind: (401, ERR_TYPE_AUTHENTICATION),
            ingress_relays_amzn_headers: false,
            ingress_relayed_response_header_names: &[],
            auth_failure_message: "authentication failed",
            uses_array_stream_shim: false,
            has_native_path_not_found: false,
            egress_stream_accept: TEXT_EVENT_STREAM,
            models_list_envelope: None,
            claims: None,
            residual_claims: None,
            residual_default: false,
            vendor_response_metadata: None,
            list_models_fingerprint_headers: &[],
            static_headers: &[],
        }
    }

    /// True when this protocol authenticates INBOUND requests with SigV4 rather than a bearer
    pub fn uses_sigv4_ingress_auth(&self) -> bool {
        matches!(self.ingress_auth, IngressAuth::SigV4)
    }

    /// This protocol's neutral computed-codec facade ([`DialectCodec`]) — the 4th seam the
    pub fn dialect(&self) -> Option<&'static dyn DialectCodec> {
        self.codec
    }
}
