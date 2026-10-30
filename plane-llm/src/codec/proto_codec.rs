// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The concrete wire-codec surface that names LLM IR types: `ProtocolReader` / `ProtocolWriter` /
//! `StreamFraming` / `PassthroughFraming`, the `Protocol` bundle and `protocol_for`, the
//! neutral-seam forwarder `DialectRef`, and the cross-protocol tool-id remap. Core keeps the
//! neutral seams and re-includes this file via `#[path]` for its test/`test-support` build, so
//! fixtures like `Protocol::anthropic()` and `protocol_for(p).reader()` keep resolving.

use busbar_contract::http::StatusCode;
// A dialect fact looked up by name is read off the plane's own declaration table
// (`crate::codec::decl_of`); the declaration shape is the contract's.
/// A lane's request-shape capabilities, handed to [`ProtocolWriter::write_request_for_lane`].
use busbar_contract::ir::egress_prep::LaneCaps;
use busbar_contract::protocol::{ArrayStreamFramer, DialectCodec, IrError};

// The six dialect names, plane-local. File-local consts so a bare `PROTO_ANTHROPIC` resolves
// identically in both compile shapes and reads as a const pattern in the `protocol_for` match.
pub const PROTO_ANTHROPIC: &str = "anthropic";
pub const PROTO_OPENAI: &str = "openai";
pub const PROTO_GEMINI: &str = "gemini";
pub const PROTO_BEDROCK: &str = "bedrock";
pub const PROTO_COHERE: &str = "cohere";
pub const PROTO_RESPONSES: &str = "responses";

use crate::codec::ir::IrStreamEvent;
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;

/// Top-level body keys the six chat dialects point-read on the pre-materialized path: `model`
/// (ingress model resolution and the pristine rewrite check), `stream` (`wants_stream`),
/// `stream_options` (the OpenAI streaming-usage opt-in) and `system` (chat's body affinity key).
/// Declared once and referenced by all six `ProtocolDecl`s; core unions each decl's `head_keys`.
/// Dialects reach it as a bare name through `use super::proto_codec::*`.
pub const LLM_CHAT_HEAD_KEYS: &[&str] = &["model", "stream", "stream_options", "system"];

// The `Protocol::<dialect>()` fixture constructors the tests name.
#[cfg(test)]
#[path = "tests/protocol_fixtures.rs"]
mod protocol_fixtures;

/// ProtocolReader extracts signals from wire responses (Stage 1a + 1b): provider-specific
/// normalizers that feed the breaker's Stage 2 classifier.
pub trait ProtocolReader: Send + Sync {
    /// Extract raw error info from HTTP response without classifying.
    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError;

    /// Classify a response into a canonical signal in one call (`extract_error` +
    /// `normalize_raw_error`). The release path runs those two stages explicitly (to apply the
    /// lane's `error_map`); this form has no production caller and only backs the per-protocol
    /// classification tests, so it is compiled only into this crate's test build (the operator
    /// `error_map` classifier is the host's, via [`crate::codec::test_host`]). Default: the two
    /// stages with an empty `error_map`.
    #[cfg(test)]
    fn classify(&self, status: StatusCode, body: &[u8]) -> CanonicalSignal {
        crate::codec::test_host::classified(&self.extract_error(status, body))
    }

    /// Read an IR request from wire JSON.
    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError>;

    /// Recover neutral token totals from a head-truncated non-stream response tail. The slice is not
    /// a well-formed document (its opening structure, or a string it cut through, is gone), so a
    /// full parse fails; instead isolate the trailing self-contained `usage` object and map this
    /// dialect's fields onto [`busbar_contract::billing::TokenUsage`]. `None` for a tail without a
    /// recognizable usage object (the caller bills zero, counted and warned). Defaults to `None`.
    fn recover_truncated_usage(
        &self,
        _tail: &[u8],
    ) -> Option<busbar_contract::billing::TokenUsage> {
        None
    }

    /// This dialect's non-stream stop-reason field, located structurally (a member name, quotes
    /// included, at a container depth; see [`crate::codec::usage_tail::StopKey`]), which the relay's
    /// incremental [`crate::codec::usage_tail::StopKeyScanner`] reads. The first such member in the
    /// body is the one read, so it must be the one [`Self::read_response`] reads. `None` (default):
    /// not scanned.
    fn stop_reason_key(&self) -> Option<crate::codec::usage_tail::StopKey> {
        None
    }

    /// Map a raw stop-reason token found under [`Self::stop_reason_key`] exactly as
    /// [`Self::read_response`] maps it. `None` where `read_response` reads no reason.
    fn stop_reason_of_token(&self, _token: &str) -> Option<crate::codec::ir::IrStopReason> {
        None
    }

    /// Read a single response/stream event from already-de-framed SSE data.
    ///
    /// Default: delegate to the fan-out [`read_response_events`] over a fresh decode state and
    /// return its first IR event. Protocols whose live path is the plural fan-out inherit this
    /// 1:1 adapter, which has no production caller there; delegating rather than a dead `None`
    /// stub means a broken call-path invariant degrades to 1:1 instead of silently dropping the
    /// event. A chunk mapping to several IR events loses the trailing ones here. Never panics:
    /// `StreamDecodeState::default()` is infallible. Anthropic overrides it with its native 1:1
    /// implementation (its plural form wraps the singular).
    fn read_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Option<IrStreamEvent> {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        self.read_response_events(host, event_type, data, &mut state)
            .into_iter()
            .next()
    }

    /// Fan-out variant: one wire event/chunk to 0..n IR stream events, threading per-request decode
    /// state (Anthropic is 1:1 and ignores it; OpenAI's flat stream synthesizes block boundaries
    /// from it). The general API the live response-translation path calls.
    fn read_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent>;

    /// The same-dialect read probe. A same-dialect stream relays bytes and reads each frame only for
    /// what the far end reported (the usage it bills) and its terminal error. This decides, from the
    /// event name (SSE `event:` line, or binary `:event-type` header) and raw payload without
    /// parsing, whether the frame can carry either; `false` skips the parse and
    /// [`Self::read_response_events`] entirely. Default `true`: every frame is read.
    fn same_proto_reads(&self, _event_type: &str, _payload: &[u8]) -> bool {
        true
    }

    /// Read a whole (non-streaming) response from wire JSON.
    fn read_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError>;

    /// IR-18: the model family that minted the reasoning signatures this reader emits as
    /// `IrDelta::SignatureDelta` on a stream (a delta has no origin of its own). The stream seam
    /// uses it to envelope a foreign signature for the client carrier (`ir::sig_envelope`).
    /// Default `None` (unknown, never enveloped).
    fn stream_signature_origin(
        &self,
        _state: &crate::codec::ir::StreamDecodeState,
    ) -> Option<crate::codec::ir::IrSignatureOrigin> {
        None
    }

    /// Clone this reader as a trait object.
    fn clone_box(&self) -> Box<dyn ProtocolReader>;

    /// What a translate attempt cannot carry (`codec::drops`), declared per dialect as data.
    fn grammar(&self) -> &'static crate::codec::drops::Grammar {
        &crate::codec::drops::Grammar::NONE
    }
}

pub trait ProtocolWriter: Send + Sync {
    /// The upstream path suffix (e.g. "/v1/messages").
    fn upstream_path(&self) -> &str;

    /// The upstream path for a specific model. Most protocols return a fixed path; Gemini's embeds
    /// the model (`/v1beta/models/{model}:generateContent`). `forward` uses it to build the URL.
    fn upstream_path_for(&self, _model: &str) -> String {
        self.upstream_path().to_string()
    }

    /// Per-request upstream path that also knows whether the caller wants a stream. Defaults to
    /// `upstream_path_for`; Gemini uses `:streamGenerateContent?alt=sse` vs `:generateContent`.
    fn upstream_path_for_stream(&self, model: &str, _stream: bool) -> String {
        self.upstream_path_for(model)
    }

    // Outbound auth is not on the writer (protocol is post-auth): the host's egress-auth unit
    // presents the lane credential under each dialect's declared `ProtocolDecl::egress_scheme`,
    // then its declared `static_headers`.

    /// Rewrites the body's `model`, returning whether the body actually changed. The default
    /// inserts/overwrites a top-level `"model"` string (Anthropic, Cohere, OpenAI, Gemini,
    /// Responses); `BedrockWriter` overrides with a no-op `false` because its model is in the URL.
    ///
    /// The return value drives request pristine-tracking (Change B): `true` only when the value
    /// truly changes, so a same-protocol passthrough already carrying the canonical model stays
    /// pristine and can short-circuit.
    fn rewrite_model_if_needed(&self, body: &mut serde_json::Value, model: &str) -> bool {
        if let Some(obj) = body.as_object_mut() {
            // Only an actual change counts: an identical `model` leaves the body pristine.
            if obj.get("model").and_then(|m| m.as_str()) == Some(model) {
                return false;
            }
            obj.insert("model".to_string(), serde_json::json!(model));
            return true;
        }
        false
    }

    /// THE REQUEST WRITE: an IR request for a known egress lane, its resolved wire `model` (`""`
    /// when unknown) and its declared [`LaneCaps`]. Both request seams
    /// (`ChatReqHandle::write_egress_request`, the plane's `encode_egress`) call it. A dialect whose
    /// spelling depends on the model family behind one wire (Bedrock, Gemini) or on the
    /// capabilities reads them; the others ignore them.
    fn write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value;

    /// The model-blind, default-capabilities write.
    fn write_request(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
    ) -> serde_json::Value {
        self.write_request_for_lane(host, req, "", &LaneCaps::default())
    }

    /// Apply a hook's `rewrite` reply to an ingress body of this dialect, in place. Returns whether
    /// the body changed; `false` leaves it untouched (fail-safe, never a corrupted request).
    ///
    /// A writer method because a reply carries `{role, content}` messages in the canonical
    /// vocabulary while each dialect frames content differently; that is write-side dialect
    /// knowledge, so a new protocol gets a correct write-back by registering, not by a new arm in a
    /// `match ingress_protocol` on the hook seam.
    ///
    /// The default is the shape three dialects share: the reply is their message shape, so its
    /// `messages` array is inserted verbatim (nothing re-derived, nothing lost) and abstract `tools`
    /// are appended. A dialect with a differently spelled container, or turns that are not
    /// `{role, content}`, overrides and re-frames.
    ///
    /// Lossy write-back (pre-existing, out of scope): a re-framing dialect renders each reply message
    /// as one text turn, so a hook echoing a projection verbatim promotes non-prose content,
    /// including the opaque-content marker, into a visible text turn sent upstream. Pinned by
    /// `apply_rewrite_to_body_echoes_redacted_marker_as_visible_text`.
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> bool {
        if !obj.get("messages").is_some_and(serde_json::Value::is_array) {
            return false;
        }
        obj.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages.to_vec()),
        );
        if !tools.is_empty() {
            match obj
                .get_mut("tools")
                .and_then(serde_json::Value::as_array_mut)
            {
                Some(existing) => existing.extend(tools.iter().cloned()),
                None => {
                    obj.insert(
                        "tools".to_string(),
                        serde_json::Value::Array(tools.to_vec()),
                    );
                }
            }
        }
        true
    }

    /// Whether this dialect's request carries an input image's fidelity (IR-08, `image_url.detail`).
    /// Without the slot it answers `false` and the egress seam warns for each image whose ask is dropped.
    fn carries_image_detail(&self) -> bool {
        true
    }

    /// Caller controls this writer drops for `req` on cross-protocol egress (no native
    /// representation). The request still forwards; the seam records each as an audit event.
    /// Default: none.
    fn dropped_egress_controls(&self, _req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        Vec::new()
    }

    /// [`Self::dropped_egress_controls`] for a lane with the declared [`LaneCaps`], the set
    /// [`Self::write_request_for_lane`] drops. Defaults to the capability-blind answer.
    fn dropped_egress_controls_for_lane(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> Vec<&'static str> {
        let _ = caps;
        self.dropped_egress_controls(req)
    }

    /// Perform the `path_base` body reshape `ProtocolDecl::reshapes_body_at_path_base` promised,
    /// returning whether the body changed. It lives with the writer so the agnostic forward path
    /// needs no knowledge of e.g. `anthropic_version`. Default: no-op, `false`.
    fn reshape_for_path_base(&self, _body: &mut serde_json::Value) -> bool {
        false
    }

    /// IR-18: whether this dialect's signature carrier reads a signature minted by `origin` as its
    /// own. A client-bound signature of another origin is wrapped in the busbar provenance envelope
    /// (`ir::sig_envelope`), which this dialect's reader unwraps. Default `true` (no carrier, never envelopes).
    fn reads_signature_origin_as_own(&self, _origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        true
    }

    /// Write a response/stream event to wire (event_type, data).
    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)>;

    /// Write one IR stream event as an ordered sequence of frames, so a dialect can emit more than
    /// one wire frame per event. The default wraps [`Self::write_response_event`] into a 0-or-1
    /// element `Vec`.
    ///
    /// `ResponsesWriter` is the sole override: a native `/v1/responses` text stream frames one
    /// `BlockStart`/`BlockStop` as several events, `output_item.added` then `content_part.added`
    /// (a strict SDK requires it before the first `output_text.delta`), and `output_text.done` /
    /// `content_part.done` before `output_item.done`. One `(event_type, data)` cannot carry that, so
    /// the seam drives streaming through this method.
    fn write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        self.write_response_event(host, ev).into_iter().collect()
    }

    /// Map a mid-stream `IrError` to a modeled-exception pair `(exception_name, message)` for
    /// protocols whose stream signals errors with an out-of-band exception frame. Only the AWS
    /// Bedrock event-stream does: an SDK dispatches off the `:message-type: exception` /
    /// `:exception-type` headers, producible only by `eventstream::encode_exception_frame`, not by
    /// `write_response_event` (always framed `:message-type: event`). `StreamTranslate` calls this for
    /// a Bedrock-ingress stream on `IrStreamEvent::Error`. Default `None`: SSE protocols carry the
    /// error in-band via `write_response_event`. Only `BedrockWriter` overrides it.
    fn write_response_exception(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _err: &IrError,
    ) -> Option<(String, String)> {
        None
    }

    /// Frame a mid-stream terminal `IrError` as this protocol's native in-band SSE stream-error
    /// event, the streaming sibling of [`Self::write_error`]. The pair `(event_type, data)` is
    /// framed like a normal event: a non-empty `event_type` becomes an `event:` line, an empty one a
    /// bare `data:` frame. The shapes differ for some protocols (Responses' `response.failed` wraps
    /// the error in a `response` object located via `event.response`, not the top-level
    /// `{"error":...}` HTTP body).
    ///
    /// The neutral seam for the host proxy's mid-stream error framer: core frames the pair without
    /// naming a stream-event type. It is what `write_response_event` produces for an error event
    /// (`None` where that declines; core falls back to its dialect-free terminal frame).
    fn write_error_frame(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        self.write_response_event(host, &IrStreamEvent::Error(err.clone()))
    }

    /// Write a whole (non-streaming) response to wire JSON.
    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value;

    /// Render a router/forward/auth-layer error as this protocol's native error envelope, so a
    /// client on the vendor's SDK gets the typed exception it expects. `status` is the HTTP status
    /// to be sent (informational; the body may embed it, e.g. Gemini's `error.code`); `kind` is a
    /// protocol-appropriate error type string (e.g. `"invalid_request_error"`, `"not_found"`);
    /// `message` is the human-readable detail.
    ///
    /// The returned JSON must be served as `application/json`.
    ///
    /// All six registered protocols override this with their native envelope. The default,
    /// `{"error":{"message":message,"type":kind}}`, is the catch-all for a future protocol that
    /// omits one (a client there would get this non-native shape).
    ///
    /// This is on the live request path, dispatched via the writer vtable from the router/auth/forward
    /// error sites (`ingress::ingress_error`, `auth`, `proxy::ingress_error`).
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "message": message,
                "type": kind,
            }
        })
    }

    /// [`Self::write_error`] with the entropy for any minted identifier handed in by the caller
    /// ([`crate::codec::write_error_envelope`]), for a caller that may not read a random source.
    /// Default: the envelope mints nothing. A dialect whose envelope carries a minted id builds it from `entropy`.
    fn write_error_from_entropy(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
        _entropy: &[u8],
    ) -> serde_json::Value {
        self.write_error(host, status, kind, message)
    }

    /// Attach protocol-specific response headers a native endpoint always carries on an error
    /// response, given the built `envelope` and canonical `kind`. Default no-op. Bedrock attaches
    /// `x-amzn-RequestId` + `x-amzn-errortype`; Anthropic mirrors the body `request_id` into the
    /// `request-id` header. `proxy::ingress_error` calls it through the writer vtable instead of
    /// branching on protocol, so the error paths cannot drift.
    fn attach_error_response_headers(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _headers: &mut busbar_contract::http::HeaderMap,
        _kind: &str,
        _envelope: &serde_json::Value,
    ) {
    }

    /// When a Bedrock-ingress client asked for a stream but the upstream answered with a buffered
    /// (non-SSE) 2xx, the translated `IrResponse` must be re-emitted as native binary eventstream
    /// frames: a `ConverseStream` decoder cannot parse a bare JSON body.
    ///
    /// Returns `Some(bytes)` when this writer needs the buffered-to-stream synthesis path, `None` when
    /// the plain translated JSON body is correct (all non-Bedrock ingress protocols). The returned bytes
    /// are the complete binary eventstream payload; the caller emits them under
    /// `writer.streaming_content_type()` so the Content-Type header matches the framing.
    ///
    /// Default: `None` (every SSE-framed protocol — a plain JSON body is acceptable).
    /// `BedrockWriter` overrides: returns `Some(bedrock_response_to_eventstream(ir, elapsed_ms))`.
    fn wrap_buffered_as_stream(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _ir: &crate::codec::ir::IrResponse,
        _elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        None
    }

    /// Inject protocol-required per-response metrics into a translated buffered body (the
    /// `serde_json::Value` from `write_response`) when timing is available. Bedrock's non-stream
    /// response always carries `metrics.latencyMs`; its `write_response` omits it (no wall clock),
    /// and the forward path injects it here, as the live path does for the `metadata` frame. Omitted,
    /// not a fabricated `0`, when timing is unavailable.
    ///
    /// Default: no-op. `BedrockWriter` inserts `metrics: { latencyMs: ms }` when `elapsed_ms` is
    /// `Some` and `value` is a JSON object.
    fn inject_response_metrics(&self, _value: &mut serde_json::Value, _elapsed_ms: Option<u64>) {}

    /// The request-id response header (name, value) to attach to a 2xx/relay response on this
    /// protocol's ingress path, or `None`. The success-path analog of
    /// `attach_error_response_headers`, dispatched via the vtable so the forward path
    /// (`maybe_attach_response_request_id`) names no protocol module.
    ///
    /// Bedrock and Anthropic both prefer the captured upstream id (a same-protocol passthrough
    /// forwards the real one) and otherwise synthesize a shape-correct one (cross-protocol passes `None`):
    /// - `BedrockWriter`: `(HDR_AMZN_REQUEST_ID, upstream-or-synth UUID)`
    /// - `AnthropicWriter`: `(HDR_REQUEST_ID, upstream-or-synth req_…)`, read by the SDK into
    ///   `APIError.request_id`.
    ///
    /// Default: `None`.
    fn ingress_response_request_id(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        None
    }

    /// The one-token "ping" probe request (`ir_encode::ping_request`) through this dialect's own
    /// `write_request`, so the probe matches a real request. The model is stamped afterward by
    /// [`Self::rewrite_model_if_needed`].
    fn probe_request(&self, host: &dyn busbar_contract::codec::CodecHost) -> serde_json::Value {
        self.write_request(host, &crate::codec::ir_encode::ping_request())
    }

    /// A minimal protocol-correct request body for an active health probe of `model`: the one-token
    /// ping serialized through [`Self::probe_request`], so no per-protocol probe code is needed.
    fn probe_body(&self, host: &dyn busbar_contract::codec::CodecHost, model: &str) -> Vec<u8> {
        let mut body = self.probe_request(host);
        let _ = self.rewrite_model_if_needed(&mut body, model);
        crate::codec::json::to_vec(&body).unwrap_or_default()
    }

    /// Build the per-stream framing state for this protocol as an ingress (client-facing) writer.
    /// `StreamTranslate` calls it once per stream and routes every protocol-specific stream-shape
    /// decision through the result, so the translator names no wire quirk. Keyed to the ingress
    /// writer because it produces the client bytes: the OpenAI per-chunk identity replay and
    /// include_usage un-fold are OpenAI-ingress only; the Bedrock messageStop/metadata deferral and
    /// finish flush are Bedrock-ingress only.
    ///
    /// Default: a no-op [`PassthroughFraming`]. `OpenAiWriter` and `BedrockWriter` override it in
    /// their own modules.
    fn new_stream_framing(&self) -> Box<dyn StreamFraming> {
        Box::new(PassthroughFraming)
    }

    /// Build this protocol's array-stream framer (the JSON-array reframer for a stream delivered as
    /// `[{...},{...}]` instead of SSE), or `None`. The forward path builds it through this method,
    /// gated by `uses_array_stream_shim()`, so it never names the gemini framer type. Default
    /// `None`; `GeminiWriter` overrides.
    fn make_array_stream_framer(&self) -> Option<Box<dyn ArrayStreamFramer>> {
        None
    }

    /// True when this ingress writer's client wants its streamed response reframed as a JSON array
    /// for request `body`. The forward path consults it (with `uses_array_stream_shim()`) rather
    /// than reading a protocol-specific body key. Default `false`; `GeminiWriter` reads its router shim key.
    fn wants_array_stream(&self, _body: &serde_json::Value) -> bool {
        false
    }

    /// The number of response candidates this ingress request asks for, when the caller set it above
    /// one. OpenAI-family reads `n`; Gemini `candidateCount` / `candidate_count`. `None` when absent,
    /// unparseable or `<= 1`, so `Some(k)` only for `k > 1`.
    ///
    /// The engine consults it at the cross-protocol request seam. `IrResponse` models one assistant
    /// turn, so a cross-protocol response can carry only candidate `[0]` and the rest would be
    /// silently dropped at HTTP 200, the least-observable data loss in an audit product. The engine
    /// rejects such a request up front (4xx). Same-protocol routes relay verbatim and do not consult it.
    fn requested_candidate_count(&self, _body: &serde_json::Value) -> Option<u64> {
        None
    }

    /// Capture the original ingress request body so this writer can mirror client-set values the
    /// spec requires on the response (OpenAI Responses: `temperature`, `top_p`, `instructions`,
    /// `metadata`, `tool_choice`, `parallel_tool_calls`, `tools`). `&self` so a stateful writer can
    /// latch it via interior mutability. Called on the buffered path (via `IrResponse::request_echo`,
    /// read by `write_response`) and the streaming path
    /// ([`crate::codec::proto_stream::StreamTranslate::set_request_echo`]). Default no-op.
    fn set_request_echo(&self, _ingress_request_body: &serde_json::Value) {}

    /// Hand this writer the translation cap its plane enforces (`limits.request_body_max_bytes`): the
    /// byte ceiling any one per-item accumulator may reach over a stream. Latched via interior
    /// mutability like [`Self::set_request_echo`]; a writer holds
    /// [`busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT`] until handed one. Default no-op.
    fn set_translate_cap(&self, _cap: usize) {}

    /// Clone this writer as a trait object.
    fn clone_box(&self) -> Box<dyn ProtocolWriter>;
}

/// Per-stream, ingress-keyed framing state for the shared [`StreamTranslate`], the vtable seam that
/// keeps the translator from naming any protocol's wire shape: the OpenAI per-chunk identity replay
/// and include_usage un-fold, and the Bedrock messageStop/metadata deferral and finish flush, live
/// behind this trait in the owning protocol's module. The translator holds one `Box<dyn
/// StreamFraming>` (from [`ProtocolWriter::new_stream_framing`]) and never branches on a protocol
/// name. The default [`PassthroughFraming`] is inert.
///
/// The translator keeps `emit_ir_event` as the emission primitive: framing methods return what to
/// emit (mutating a chunk in place, or returning IR events / a trailing chunk), preserving the exact
/// byte-level emission order.
pub trait StreamFraming: Send {
    /// Egress-chunk seam (OpenAI ingress), called for every reframed `chat.completion.chunk` just
    /// before framing. (a) Replays the latched stream identity (`id`/`created`/`model`) onto `chunk`
    /// in place (the opening chunk latches it; later chunks lack it), so the stream shares one id as
    /// a genuine OpenAI stream does. (b) For a usage-bearing finish chunk, removes the folded `usage`
    /// from `chunk` and returns it as a separate trailing usage-only chunk (the include_usage
    /// un-fold), framed after `chunk`.
    ///
    /// Default ([`PassthroughFraming`]): no mutation, `None`.
    fn on_egress_chunk(&mut self, _chunk: &mut serde_json::Value) -> Option<serde_json::Value> {
        None
    }

    /// Combined-stop-delta seam (Bedrock ingress), called on a combined
    /// `MessageDelta{stop_reason: Some, usage}`. Returns, in order, the IR events to emit via
    /// `emit_ir_event` to reproduce ConverseStream's two-frame stop/metadata split while ensuring
    /// exactly one `metadata` frame per stream: always a stop-only delta (`messageStop`), plus a
    /// usage-only delta (`metadata`) iff real usage rode with the stop (else the metadata is deferred
    /// to a trailing usage-only delta or `on_finish`). On `Some` the translator consumes the original event.
    ///
    /// Default ([`PassthroughFraming`]): `None`, so the translator takes its normal path.
    fn on_combined_stop_delta(
        &mut self,
        _stop_reason: crate::codec::ir::IrStopReason,
        _stop_sequence: Option<String>,
        _usage: &crate::codec::ir::IrUsage,
    ) -> Option<Vec<crate::codec::ir::IrStreamEvent>> {
        None
    }

    /// Usage-only-delta seam (Bedrock ingress), called for a trailing `MessageDelta{stop_reason:
    /// None}` (the OpenAI include_usage chunk, or a native usage frame). `true` emits it as the
    /// stream's single `metadata` frame; `false` suppresses it (a `metadata` already rode with the
    /// stop). Updates state to keep the one-metadata invariant and resolve any deferral.
    ///
    /// Default ([`PassthroughFraming`]): `None`, so this delta is not special-cased.
    fn on_usage_only_delta(&mut self) -> Option<bool> {
        None
    }

    /// Finish seam (Bedrock ingress), called once at end of stream. `Some(event)` when a `metadata`
    /// frame was deferred (a zero-usage stop with no trailing usage delta, the default OpenAI
    /// streaming case) and never resolved: the translator flushes one best-effort zero-usage
    /// `metadata` frame to honor the always-one-metadata invariant. `None` when no flush is owed.
    ///
    /// Default ([`PassthroughFraming`]): `None`.
    fn on_finish(&mut self) -> Option<crate::codec::ir::IrStreamEvent> {
        None
    }

    /// Metadata-metrics seam (Bedrock ingress), called in the eventstream branch for each frame with
    /// its event-type, built data object, and the milliseconds since the stream's first byte on the
    /// translator's clock (`None`: timing unavailable). The Bedrock impl injects
    /// `metrics.latencyMs` into the `metadata` frame in place, omitting `metrics` rather than
    /// emitting a tell-tale `0` when timing is unavailable. Keeps the event-type literal and latency
    /// shape out of the translator.
    ///
    /// Default ([`PassthroughFraming`]): no-op.
    fn inject_streaming_metrics(
        &self,
        _event_type: &str,
        _data: &mut serde_json::Value,
        _elapsed_ms: Option<u64>,
    ) {
    }

    /// Stream-abort seam (eventstream ingress): the error type name this ingress emits as the
    /// terminal frame on an aborted stream (buffer overflow / malformed prelude). `Some(name)`
    /// (Bedrock: `InternalServerException`) makes the translator emit a well-formed eventstream
    /// exception frame without naming the wire type; `None` (default, every SSE protocol) takes the
    /// SSE-abort path.
    fn abort_exception_type(&self) -> Option<&'static str> {
        None
    }

    /// Terminal-usage-fold seam (plain SSE ingresses: Anthropic/Gemini/Cohere/Responses). These carry
    /// usage in one terminal `message_delta`. When the egress reports it in a separate trailing chunk
    /// after the finish chunk (OpenAI `include_usage`), the terminal frame would ship zeros and the
    /// post-stop guard would drop the real usage. `true` makes the translator defer the terminal
    /// `MessageDelta`/`MessageStop`, merge trailing usage, and flush at end of stream (also through
    /// the json-array framer, see `proxy::response_body`).
    ///
    /// Default `true` ([`PassthroughFraming`]). OpenAI overrides to `false` (it un-folds, re-emitting
    /// the separate usage chunk via `on_egress_chunk`), as does Bedrock (separate `metadata` frame,
    /// via `on_combined_stop_delta`/`on_usage_only_delta`).
    fn folds_terminal_usage(&self) -> bool {
        true
    }

    /// Client-intent seam (OpenAI ingress): whether the original client request had
    /// `stream_options.include_usage == true`. Busbar always injects `include_usage` upstream to bill
    /// streaming calls, but a native OpenAI stream emits the trailing usage chunk only if the client
    /// opted in, and a client that did not would hit an `choices[0]` IndexError on an unsolicited
    /// `{choices:[], usage}` chunk. So `false` makes the OpenAI framing strip the folded usage; `true`
    /// un-folds it to the native trailing chunk. Billing reads the IR-side `last_usage`, unaffected.
    ///
    /// Default ([`PassthroughFraming`], non-OpenAI ingress): no-op.
    fn set_client_include_usage(&mut self, _include: bool) {}

    /// Same-protocol edit seam (OpenAI ingress). The same-protocol path re-emits each upstream frame
    /// byte for byte and never routes it through [`on_egress_chunk`]. Busbar forces
    /// `stream_options.include_usage` upstream (to bill), so for a client that did not opt in two
    /// frames are tells a direct stream never carries:
    ///
    /// - the native trailing usage-only chunk (a usage object beside an empty `choices`), which a
    ///   strict SDK `choices[0]`-IndexErrors on: [`SameProtoEdit::Drop`];
    /// - `"usage":null` on every content/finish chunk (a top-level `usage` beside a non-empty
    ///   `choices`), which a direct opted-out stream omits: [`SameProtoEdit::StripUsage`] removes
    ///   that member by a byte-level edit, not a DOM re-serialize.
    ///
    /// Decided on the raw JSON `payload`, read as its parse would (last of a repeated member, escaped
    /// keys unescaped); the caller applies an edit only to a payload that parses. Billing is
    /// unaffected (the usage side channel reads the frame before any edit). The opted-in client and
    /// every other frame are [`SameProtoEdit::Keep`].
    ///
    /// Default ([`PassthroughFraming`], non-OpenAI ingress): [`SameProtoEdit::Keep`].
    fn same_proto_edit(&self, _payload: &[u8]) -> SameProtoEdit {
        SameProtoEdit::Keep
    }
}

/// What a same-protocol relay does to one upstream frame on its way to the client
/// ([`StreamFraming::same_proto_edit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameProtoEdit {
    /// Re-emit the frame verbatim.
    Keep,
    /// Drop the whole frame.
    Drop,
    /// Re-emit the frame with its top-level `usage` member removed.
    StripUsage,
}

/// A stream writer's per-stream state behind one lock. Writers are `&self` and `Sync` (the
/// [`ProtocolWriter`] trait), so inter-event state lives here. A clone carries the state (a
/// mid-stream `Protocol::clone` is the same stream). A poisoned lock is read through: the state
/// holds no invariant a panicking holder breaks.
pub struct StreamCell<T>(std::sync::Mutex<T>);

impl<T> StreamCell<T> {
    /// The cell over `state` (a `const` writer builds its fresh state here).
    pub const fn new(state: T) -> Self {
        StreamCell(std::sync::Mutex::new(state))
    }

    /// The state, locked.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: Clone> Clone for StreamCell<T> {
    fn clone(&self) -> Self {
        StreamCell::new(self.lock().clone())
    }
}

/// Inert default [`StreamFraming`] for every protocol whose ingress stream has no per-stream
/// framing quirk (Anthropic/Gemini/Cohere/Responses). Holds no state.
pub struct PassthroughFraming;

impl StreamFraming for PassthroughFraming {}

/// Bundled Protocol with name + reader + writer.
pub struct Protocol {
    name: &'static str,
    // `pub(crate)` so the fidelity/round-trip tests reach the reader/writer by field; the
    // `.reader()`/`.writer()` accessors remain the public surface.
    pub reader: Box<dyn ProtocolReader>,
    pub writer: Box<dyn ProtocolWriter>,
}

impl Clone for Box<dyn ProtocolReader> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl Clone for Box<dyn ProtocolWriter> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl Clone for Protocol {
    fn clone(&self) -> Self {
        Protocol {
            name: self.name,
            reader: self.reader.clone(),
            writer: self.writer.clone(),
        }
    }
}

impl Protocol {
    pub fn new<R, W>(name: &'static str, reader: R, writer: W) -> Self
    where
        R: ProtocolReader + 'static,
        W: ProtocolWriter + 'static,
    {
        Self {
            name,
            reader: Box::new(reader),
            writer: Box::new(writer),
        }
    }

    /// The protocol name as the registry's interned `&'static str`, so a caller outliving the
    /// borrowed lookup key (e.g. a stream body holding the ingress protocol) holds it without allocating.
    pub fn name_static(&self) -> &'static str {
        self.name
    }

    /// This protocol's declaration (the constant facts core reads by field). Always `Some` for a
    /// registered codec protocol; the `Option` mirrors [`crate::codec::decl_of`].
    pub fn decl(&self) -> Option<&'static busbar_contract::protocol::ProtocolDecl> {
        crate::codec::decl_of(self.name)
    }

    /// Returns the reader for this protocol.
    pub fn reader(&self) -> &dyn ProtocolReader {
        self.reader.as_ref()
    }

    /// Returns the writer for this protocol.
    pub fn writer(&self) -> &dyn ProtocolWriter {
        self.writer.as_ref()
    }
}

/// Build this protocol's wire codec by name, a `Protocol` instance for paths that translate.
///
/// Name resolution reads the plane's own table ([`crate::codec::decl_of`], no allocation). The codec
/// itself allocates and must: `GeminiWriter`, `CohereWriter` and `ResponsesWriter` carry per-stream
/// mutable state and must not be shared across concurrent requests. Callers holding a resolution
/// for a stream's life resolve once and keep it (see `FirstByteBody::new`).
///
/// A caller wanting a by-name constant must not come here: such facts (streaming content type, shim
/// key, tool-id prefix, ingress auth scheme) are `ProtocolDecl` fields read through `decl_for` with
/// no allocation, whereas this costs two `Box`es.
///
/// `None` for an undeclared name and for a protocol with no codec (MCP).
pub fn protocol_for(name: &str) -> Option<Protocol> {
    // `ProtocolDecl.codec` is the neutral `DialectCodec` factory (core names no `Protocol`), so the
    // name-to-codec map lives here with the dialects. Each [`DialectEntry::protocol`] builds a fresh
    // instance per resolution (writers carry per-stream state). MCP has no codec: `None`.
    entry_of(name).map(|d| (d.protocol)())
}

/// One dialect's registration: everything this plane resolves by name, declared in the dialect's
/// own module. The one list is [`crate::codec::DIALECTS`]; a new dialect is a new module plus one
/// line there, and nothing in this file names a dialect.
pub struct DialectEntry {
    /// The dialect's declaration (its name is the registry key).
    pub decl: &'static busbar_contract::protocol::ProtocolDecl,
    /// Build the dialect's wire codec: a fresh instance per resolution ([`protocol_for`]).
    pub protocol: fn() -> Protocol,
    /// Hand a fresh writer, built on the stack, to the callback ([`with_writer`]).
    pub with_writer: WithWriter,
    /// Hand the (stateless) reader to the callback ([`with_reader`]).
    pub with_reader: WithReader,
    /// The dialect's row of the leaf-op `(operation, protocol)` dispatch.
    pub leaf: &'static crate::codec::leaf_codec::LeafCodecs,
}

/// A dialect's stack-writer callback: builds a fresh writer and hands it to the closure.
pub type WithWriter = fn(&mut dyn FnMut(&dyn ProtocolWriter));

/// A dialect's reader callback: hands its (stateless) reader to the closure.
pub type WithReader = fn(&mut dyn FnMut(&dyn ProtocolReader));

/// The registration of the dialect named `name`, read off [`crate::codec::DIALECTS`]. `None` for a
/// name this plane does not register.
pub fn entry_of(name: &str) -> Option<&'static DialectEntry> {
    crate::codec::DIALECTS
        .iter()
        .copied()
        .find(|d| d.decl.name == name)
}

/// Run `f` against the named dialect's writer built on the stack, with no `Protocol` and no `Box`.
///
/// Writers carry per-stream state, so `Protocol::new`'s `Box::new(writer)` is a heap allocation.
/// The neutral [`DialectRef`] methods are stateless one-shot calls on the request hot path (model
/// rewrite, path-base reshape, error envelopes), so they take a fresh stack writer: each dialect's
/// `const` writer is pristine, its empty sets and `None` slots allocate nothing, and it drops at
/// the closing brace. `None` for a name with no codec (MCP), as `protocol_for` answers.
///
/// This is the allocation-free way to ask a dialect writer a stateless question, and it is public so
/// callers outside this crate (the plane) can ask without paying for a `Protocol`. Every call in
/// the closure sees one writer, so a two-call question (write the request, then rewrite the model)
/// is still one instance and nothing on the heap.
///
/// A caller keeping per-stream state across closures (a stream translator seeing the same writer
/// frame after frame) must use [`protocol_for`] instead.
pub fn with_writer<T>(name: &str, f: impl FnOnce(&dyn ProtocolWriter) -> T) -> Option<T> {
    let entry = entry_of(name)?;
    let mut f = Some(f);
    let mut out = None;
    (entry.with_writer)(&mut |w| out = f.take().map(|f| f(w)));
    out
}

/// The reader twin of [`with_writer`]: the named dialect's reader on the stack (readers are unit
/// structs, so this is a pure dispatch). Public for the same reason: a reader question through
/// `protocol_for` pays the writer's box for nothing. Readers hold nothing (a stream's state is the
/// caller's `StreamDecodeState`), so every reader question is stateless and belongs here.
pub fn with_reader<T>(name: &str, f: impl FnOnce(&dyn ProtocolReader) -> T) -> Option<T> {
    let entry = entry_of(name)?;
    let mut f = Some(f);
    let mut out = None;
    (entry.with_reader)(&mut |r| out = f.take().map(|f| f(r)));
    out
}

/// The plane-local equivalent of the former core `handlers::protocol_error` round trip: the shared
/// upstream-error-envelope extractor, by name. Every operation cell of a dialect answers
/// [`OperationHandler::extract_error`] through it, so the upstream error vocabulary is stated once,
/// in the protocol's reader (same `extract_error` path and status-only fallback as before).
pub fn protocol_error(
    name: &str,
    status: u16,
    body: &[u8],
) -> busbar_contract::upstream::RawUpstreamError {
    with_reader(name, |r| {
        r.extract_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body,
        )
    })
    .unwrap_or_else(|| busbar_contract::upstream::RawUpstreamError::from_status(status))
}

pub struct DialectRef(&'static str);

/// Construct the neutral [`DialectCodec`] facade for a named dialect, `const` so each dialect's
/// `ProtocolDecl::codec` can point at a `static` of it. Core reads it through
/// `decl_for(name).dialect()` as a `&'static dyn` borrow, naming nothing concrete; being a static,
/// it allocates nothing (a pure-memory dispatch, like `ProtocolDecl::handler`).
pub const fn dialect_ref(name: &'static str) -> DialectRef {
    DialectRef(name)
}

impl DialectCodec for DialectRef {
    fn probe_body(&self, host: &dyn busbar_contract::codec::CodecHost, model: &str) -> Vec<u8> {
        with_writer(self.0, |w| w.probe_body(host, model)).unwrap_or_default()
    }
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> bool {
        with_writer(self.0, |w| {
            w.apply_rewrite_to_ingress_body(obj, messages, tools)
        })
        .unwrap_or(false)
    }
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        with_reader(self.0, |r| r.recover_truncated_usage(tail)).flatten()
    }
    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        with_writer(self.0, |w| {
            w.ingress_response_request_id(host, upstream_request_id)
        })
        .flatten()
    }
    fn write_error(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        with_writer(self.0, |w| w.write_error(host, status, kind, message)).unwrap_or_default()
    }
    fn requested_candidate_count(&self, body: &serde_json::Value) -> Option<u64> {
        with_writer(self.0, |w| w.requested_candidate_count(body)).flatten()
    }
    fn write_response_exception(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, String)> {
        with_writer(self.0, |w| w.write_response_exception(host, err)).flatten()
    }
    fn write_error_frame(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        with_writer(self.0, |w| w.write_error_frame(host, err)).flatten()
    }
    fn wants_array_stream(&self, body: &serde_json::Value) -> bool {
        with_writer(self.0, |w| w.wants_array_stream(body)).unwrap_or(false)
    }
    fn inject_response_metrics(&self, value: &mut serde_json::Value, elapsed_ms: Option<u64>) {
        with_writer(self.0, |w| w.inject_response_metrics(value, elapsed_ms));
    }
    fn error_response_fields(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        kind: &str,
        envelope: &serde_json::Value,
    ) -> Vec<(String, Vec<u8>)> {
        let mut headers = busbar_contract::http::HeaderMap::new();
        with_writer(self.0, |w| {
            w.attach_error_response_headers(host, &mut headers, kind, envelope)
        });
        headers
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
            .collect()
    }
    fn extract_error(
        &self,
        status: u16,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        protocol_error(self.0, status, body)
    }
    fn make_array_stream_framer(&self) -> Option<Box<dyn ArrayStreamFramer>> {
        with_writer(self.0, |w| w.make_array_stream_framer()).flatten()
    }
    fn upstream_path_for_stream(&self, model: &str, stream: bool) -> String {
        with_writer(self.0, |w| w.upstream_path_for_stream(model, stream)).unwrap_or_default()
    }
    fn rewrite_model_if_needed(&self, body: &mut serde_json::Value, model: &str) -> bool {
        with_writer(self.0, |w| w.rewrite_model_if_needed(body, model)).unwrap_or(false)
    }
    fn reshape_for_path_base(&self, body: &mut serde_json::Value) -> bool {
        with_writer(self.0, |w| w.reshape_for_path_base(body)).unwrap_or(false)
    }
}

/// The ingress protocol's native tool-call id prefix, used by [`ToolIdRemap`] to reshape a foreign
/// egress tool id. `None` means no tool id on the wire (Gemini correlates by name) or a free-form id
/// with no canonical prefix (Cohere); the foreign id then passes through verbatim.
pub fn native_tool_id_prefix(protocol_name: &str) -> Option<&'static str> {
    crate::codec::decl_of(protocol_name).and_then(|d| d.native_tool_id_prefix)
}

/// Marker segment in a busbar-minted tool id, so the request-side reverse can tell a reshaped id from
/// a client-authored one and recover the original egress id with no cross-request state.
/// Alphanumeric (valid in every native id shape) and unlikely to prefix a real client id. The
/// original id follows as lower hex, making the transform a pure deterministic bijection: the same
/// egress id always maps to the same native id, within a request and across rounds.
pub const TOOL_ID_REMAP_MARKER: &str = "bb1";

/// Per-request / per-stream tool-id remap, applied only at the cross-protocol seam (ingress !=
/// egress); same-protocol passthrough never constructs one.
///
/// Forward (response): each foreign egress tool id becomes `<prefix><MARKER><hex(egress_id)>`, so
/// an OpenAI `call_…` reaches an Anthropic client as a native `toolu_…` (a foreign one is a proxy
/// tell). The in-request map only memoizes; the encoding is deterministic regardless.
///
/// Reverse (next request): the client echoes the native id in a `tool_result`;
/// [`decode_native_tool_id`] strips the marker and hex-decodes it to the original egress id. An id
/// without the marker is client-authored (or same-protocol) and passes through.
#[derive(Default)]
pub struct ToolIdRemap {
    map: std::collections::HashMap<String, String>,
}

/// Bound on memoized egress-to-native tool-id pairs per [`ToolIdRemap`]. The map only memoizes a
/// pure deterministic encode, so refusing inserts past the cap changes no answer, only retention.
/// Same scale as the readers' `MAX_OPEN_TOOLS` (OpenAI's parallel-tool-call limit), the distinct
/// ids a legitimate turn carries; more is an untrusted upstream growing a per-request map.
pub const TOOL_ID_REMAP_MAX_MEMO: usize = crate::codec::dialect::MAX_OPEN_TOOL_CALLS;

impl ToolIdRemap {
    /// Reshape one egress tool id into the ingress protocol's native form (deterministic, memoized).
    /// A `None` ingress prefix (Gemini drops tool ids; Cohere ids are free-form, so a reversible
    /// reshape could collide with client-authored ids) returns the id unchanged.
    pub fn native_for(&mut self, ingress_protocol: &str, egress_id: &str) -> String {
        let Some(prefix) = native_tool_id_prefix(ingress_protocol) else {
            return egress_id.to_string();
        };
        if let Some(existing) = self.map.get(egress_id) {
            return existing.clone();
        }
        let native = format!("{prefix}{TOOL_ID_REMAP_MARKER}{}", ::hex::encode(egress_id));
        // Bounded retention: the encode is pure, so past the cap the same native id is returned, just
        // not remembered, and `decode_native_tool_id` still reverses it. Uncapped, the map grows for
        // the stream's life keyed by upstream-chosen strings (the Anthropic reader has no per-stream
        // `tool_use` limit, unlike the other readers' `MAX_OPEN_TOOLS`). Stop inserting rather than
        // evict: eviction buys nothing and an LRU adds state to keep correct.
        if self.map.len() < TOOL_ID_REMAP_MAX_MEMO {
            self.map.insert(egress_id.to_string(), native.clone());
        }
        native
    }

    /// Rewrite every tool id in a non-stream `IrResponse` to the ingress-native form (in place).
    pub fn remap_response(
        &mut self,
        ingress_protocol: &str,
        ir: &mut crate::codec::ir::IrResponse,
    ) {
        for block in &mut ir.content {
            self.remap_block(ingress_protocol, block);
        }
    }

    /// Rewrite every tool id in a streaming `IrStreamEvent` to the ingress-native form (in place).
    pub fn remap_event(
        &mut self,
        ingress_protocol: &str,
        event: &mut crate::codec::ir::IrStreamEvent,
    ) {
        if let crate::codec::ir::IrStreamEvent::BlockStart {
            block: crate::codec::ir::IrBlockMeta::ToolUse { id, .. },
            ..
        } = event
        {
            *id = self.native_for(ingress_protocol, id);
        }
    }

    pub fn remap_block(&mut self, ingress_protocol: &str, block: &mut crate::codec::ir::IrBlock) {
        match block {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => {
                *id = self.native_for(ingress_protocol, id);
            }
            crate::codec::ir::IrBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                *tool_use_id = self.native_for(ingress_protocol, tool_use_id);
                for inner in content {
                    self.remap_block(ingress_protocol, inner);
                }
            }
            crate::codec::ir::IrBlock::Text { .. }
            | crate::codec::ir::IrBlock::Thinking { .. }
            | crate::codec::ir::IrBlock::Image { .. }
            | crate::codec::ir::IrBlock::Media { .. }
            | crate::codec::ir::IrBlock::Json(_)
            | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
        }
    }
}

/// Recover the original egress tool id from a busbar-reshaped native id (the exact reverse of
/// [`ToolIdRemap::native_for`]): `Some(original)` when `id` carries the marker after the ingress
/// protocol's own native prefix and the hex tail decodes to valid UTF-8, else `None` (client-
/// authored, pass through). Pure and stateless.
///
/// Gated on the same `native_tool_id_prefix(ingress_protocol)` the encode used, not a scan of every
/// known prefix: foreign prefixes would mis-detect a client id of shape
/// `<any-known-prefix>bb1<even-len-hex>` and corrupt the tool_use/tool_result correlation. A
/// prefix-less ingress (Cohere, Gemini) returns `None`.
pub fn decode_native_tool_id(ingress_protocol: &str, id: &str) -> Option<String> {
    // The ingress's own native prefix, as `native_for` prepended it. A prefix-less protocol never
    // has ids reshaped, so nothing to decode.
    let prefix = native_tool_id_prefix(ingress_protocol)?;
    let rest = id.strip_prefix(prefix)?;
    let hexpart = rest.strip_prefix(TOOL_ID_REMAP_MARKER)?;
    // A marker-only id (empty tail) is client-authored `<prefix>bb1`: `native_for` always
    // hex-encodes. Decoding would yield an empty string and break the inverse, so pass it through.
    if hexpart.is_empty() {
        return None;
    }
    // A busbar id has an even-length lowercase-hex tail. `hex::decode` accepts uppercase but
    // `native_for` emits only lowercase, so an uppercase or mixed tail is client-authored; guard it
    // out before decoding or a client id that decodes to valid UTF-8 would be mangled.
    if !hexpart
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let bytes = ::hex::decode(hexpart).ok()?;
    String::from_utf8(bytes).ok()
}

/// Walk a request-body IR (messages, blocks, recursing into `ToolResult.content`) and decode any
/// busbar-reshaped tool id to the original egress id, so a `tool_result` echoed after a
/// cross-protocol response references the id the egress backend issued. A no-op for ids without the
/// marker. Applied at the request seam (ingress != egress) after `read_request`, before the egress `write_request`.
pub fn decode_request_tool_ids(
    ingress_protocol: &str,
    messages: &mut [crate::codec::ir::IrMessage],
) {
    fn walk(ingress_protocol: &str, block: &mut crate::codec::ir::IrBlock) {
        match block {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => {
                if let Some(orig) = decode_native_tool_id(ingress_protocol, id) {
                    *id = orig;
                }
            }
            crate::codec::ir::IrBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                if let Some(orig) = decode_native_tool_id(ingress_protocol, tool_use_id) {
                    *tool_use_id = orig;
                }
                for inner in content {
                    walk(ingress_protocol, inner);
                }
            }
            crate::codec::ir::IrBlock::Text { .. }
            | crate::codec::ir::IrBlock::Thinking { .. }
            | crate::codec::ir::IrBlock::Image { .. }
            | crate::codec::ir::IrBlock::Media { .. }
            | crate::codec::ir::IrBlock::Json(_)
            | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
        }
    }
    for msg in messages {
        for block in &mut msg.content {
            walk(ingress_protocol, block);
        }
    }
}
