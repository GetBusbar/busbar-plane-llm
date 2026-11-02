// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The concrete wire-codec surface that names LLM IR types: `ProtocolReader` / `ProtocolWriter` /

use busbar_contract::http::StatusCode;

/// A lane's request-shape capabilities, handed to [`ProtocolWriter::write_request_for_lane`].
use crate::codec::egress_prep::LaneCaps;
use busbar_contract::protocol::{ArrayStreamFramer, DialectCodec, IrError};

pub const PROTO_ANTHROPIC: &str = "anthropic";
pub const PROTO_OPENAI: &str = "openai";
pub const PROTO_GEMINI: &str = "gemini";
pub const PROTO_BEDROCK: &str = "bedrock";
pub const PROTO_COHERE: &str = "cohere";
pub const PROTO_RESPONSES: &str = "responses";

use crate::codec::ir::IrStreamEvent;

pub(crate) fn entropy_error() -> IrError {
    IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("identifier entropy unavailable".into()),
        retry_after: None,
    }
}

/// Preserve a writer failure across the contract's infallible buffered-stream handle seam.
pub(crate) struct CheckedEntropy<'a> {
    host: &'a dyn busbar_contract::codec::CodecHost,
    failed: std::sync::atomic::AtomicBool,
}

impl<'a> CheckedEntropy<'a> {
    pub(crate) fn new(host: &'a dyn busbar_contract::codec::CodecHost) -> Self {
        Self {
            host,
            failed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl busbar_contract::codec::CodecHost for CheckedEntropy<'_> {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        if self.failed() || !self.host.fill_entropy(out) {
            self.failed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            false
        } else {
            true
        }
    }
    fn wall_clock_now(&self) -> Option<u64> {
        self.host.wall_clock_now()
    }
    fn monotonic_ns(&self) -> Option<u64> {
        self.host.monotonic_ns()
    }
}
#[cfg(test)]
use busbar_contract::upstream::CanonicalSignal;

/// Top-level body keys the six chat dialects point-read on the pre-materialized path: `model`
pub const LLM_CHAT_HEAD_KEYS: &[&str] = &["model", "stream", "stream_options", "system"];

#[cfg(test)]
#[path = "tests/protocol_fixtures.rs"]
mod protocol_fixtures;

/// Reported stream accounting whose unrelated content metadata could not become an IR event.
/// This carries no content, role, identity, stop reason or success indication.
#[derive(Debug)]
pub enum ReportedStreamUsage {
    /// Counts reported at stream start, retained for later terminal usage backfill.
    Start(crate::codec::ir::IrUsage),
    /// A later cumulative report, folded without creating a terminal content event.
    Trailing(crate::codec::ir::IrUsage),
}

/// ProtocolReader extracts signals from wire responses (Stage 1a + 1b): provider-specific
pub trait ProtocolReader: Send + Sync {
    /// Extract raw error info from HTTP response without classifying.
    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError;

    /// Extract with response heads where the wire protocol carries its identity there.
    fn extract_error_with_head(
        &self,
        status: StatusCode,
        _head: busbar_contract::protocol::HeadFields<'_>,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        self.extract_error(status, body)
    }

    /// Classify a response into a canonical signal in one call (`extract_error` +
    #[cfg(test)]
    fn classify(&self, status: StatusCode, body: &[u8]) -> CanonicalSignal {
        crate::codec::test_host::classified(&self.extract_error(status, body))
    }

    /// Read an IR request from wire JSON.
    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError>;

    /// Recover neutral token totals from the original prefix of a non-stream response cut by its
    fn recover_truncated_usage(
        &self,
        _tail: &[u8],
    ) -> Option<busbar_contract::billing::TokenUsage> {
        None
    }

    /// This dialect's non-stream stop-reason field, located structurally (a member name, quotes
    fn stop_reason_key(&self) -> Option<crate::codec::usage_tail::StopKey> {
        None
    }

    /// Map a raw stop-reason token found under [`Self::stop_reason_key`] exactly as
    fn stop_reason_of_token(&self, _token: &str) -> Option<crate::codec::ir::IrStopReason> {
        None
    }

    /// Independently read a complete response's reported usage after its content failed decoding.
    fn recover_response_usage(
        &self,
        _body: &serde_json::Value,
    ) -> Option<busbar_contract::billing::TokenUsage> {
        None
    }

    /// Independently read native reported work when a parsed stream frame produced no
    fn recover_stream_usage(
        &self,
        _event_type: &str,
        _data: &serde_json::Value,
    ) -> Option<ReportedStreamUsage> {
        None
    }

    /// Read a single response/stream event from already-de-framed SSE data.
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
    fn read_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent>;

    /// The same-dialect read probe. A same-dialect stream relays bytes and reads each frame only for
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
    fn upstream_path_for(&self, _model: &str) -> String {
        self.upstream_path().to_string()
    }

    /// Per-request upstream path that also knows whether the caller wants a stream. Defaults to
    fn upstream_path_for_stream(&self, model: &str, _stream: bool) -> String {
        self.upstream_path_for(model)
    }

    /// Rewrites the body's `model`, returning whether the body actually changed. The default
    fn rewrite_model_if_needed(&self, body: &mut serde_json::Value, model: &str) -> bool {
        if let Some(obj) = body.as_object_mut() {
            if obj.get("model").and_then(|m| m.as_str()) == Some(model) {
                return false;
            }
            obj.insert("model".to_string(), serde_json::json!(model));
            return true;
        }
        false
    }

    /// THE REQUEST WRITE: an IR request for a known egress lane, its resolved wire `model` (`""`
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
    fn carries_image_detail(&self) -> bool {
        true
    }

    /// Caller controls this writer drops for `req` on cross-protocol egress (no native
    fn dropped_egress_controls(&self, _req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        Vec::new()
    }

    /// [`Self::dropped_egress_controls`] for a lane with the declared [`LaneCaps`], the set
    fn dropped_egress_controls_for_lane(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> Vec<&'static str> {
        let _ = caps;
        self.dropped_egress_controls(req)
    }

    /// Perform the `path_base` body reshape `ProtocolDecl::reshapes_body_at_path_base` promised,
    fn reshape_for_path_base(&self, _body: &mut serde_json::Value) -> bool {
        false
    }

    /// IR-18: whether this dialect's signature carrier reads a signature minted by `origin` as its
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
    fn write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        self.write_response_event(host, ev).into_iter().collect()
    }

    /// Fallible encoding for writers that must synthesize an identity. An error contributes no
    fn try_write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, IrError> {
        Ok(self.write_response_events(host, ev))
    }

    fn try_write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        model: &str,
        caps: &LaneCaps,
    ) -> Result<serde_json::Value, IrError> {
        Ok(self.write_request_for_lane(host, req, model, caps))
    }

    fn try_write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> Result<serde_json::Value, IrError> {
        Ok(self.write_response(host, resp))
    }

    /// Render a local encoding refusal independently of stream identity. A dialect's optional
    fn encoding_failure(&self, host: &dyn busbar_contract::codec::CodecHost) -> serde_json::Value {
        self.write_error(host, 500, "server_error", "identifier entropy unavailable")
    }

    /// Map a mid-stream `IrError` to a modeled-exception pair `(exception_name, message)` for
    fn write_response_exception(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _err: &IrError,
    ) -> Option<(String, String)> {
        None
    }

    /// Frame a mid-stream terminal `IrError` as this protocol's native in-band SSE stream-error
    fn write_error_frame(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        match self.try_write_response_events(host, &IrStreamEvent::Error(err.clone())) {
            Ok(frames) => frames.into_iter().next(),
            Err(_) => Some((
                "error".into(),
                self.write_error(
                    host,
                    500,
                    "server_error",
                    err.provider_signal
                        .as_deref()
                        .unwrap_or("upstream stream error"),
                ),
            )),
        }
    }

    /// Write a whole (non-streaming) response to wire JSON.
    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value;

    /// Render a router/forward/auth-layer error as this protocol's native error envelope, so a
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
    fn attach_error_response_headers(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _headers: &mut busbar_contract::http::HeaderMap,
        _kind: &str,
        _envelope: &serde_json::Value,
    ) {
    }

    /// When a Bedrock-ingress client asked for a stream but the upstream answered with a buffered
    fn wrap_buffered_as_stream(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _ir: &crate::codec::ir::IrResponse,
        _elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        None
    }

    /// Inject protocol-required per-response metrics into a translated buffered body (the
    fn inject_response_metrics(&self, _value: &mut serde_json::Value, _elapsed_ms: Option<u64>) {}

    /// The request-id response header (name, value) to attach to a 2xx/relay response on this
    fn ingress_response_request_id(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        None
    }

    /// The one-token "ping" probe request (`ir_encode::ping_request`) through this dialect's own
    fn probe_request(&self, host: &dyn busbar_contract::codec::CodecHost) -> serde_json::Value {
        self.write_request(host, &crate::codec::ir_encode::ping_request())
    }

    /// A minimal protocol-correct request body for an active health probe of `model`: the one-token
    fn probe_body(&self, host: &dyn busbar_contract::codec::CodecHost, model: &str) -> Vec<u8> {
        let mut body = self.probe_request(host);
        let _ = self.rewrite_model_if_needed(&mut body, model);
        crate::codec::json::to_vec(&body).unwrap_or_default()
    }

    /// Build the per-stream framing state for this protocol as an ingress (client-facing) writer.
    fn new_stream_framing(&self) -> Box<dyn StreamFraming> {
        Box::new(PassthroughFraming)
    }

    /// Build this protocol's array-stream framer (the JSON-array reframer for a stream delivered as
    fn make_array_stream_framer(&self) -> Option<Box<dyn ArrayStreamFramer>> {
        None
    }

    /// True when this ingress writer's client wants its streamed response reframed as a JSON array
    fn wants_array_stream(&self, _body: &serde_json::Value) -> bool {
        false
    }

    /// The number of response candidates this ingress request asks for, when the caller set it above
    fn requested_candidate_count(&self, _body: &serde_json::Value) -> Option<u64> {
        None
    }

    /// Capture the original ingress request body so this writer can mirror client-set values the
    fn set_request_echo(&self, _ingress_request_body: &serde_json::Value) {}

    /// Hand this writer the translation cap its plane enforces (`limits.request_body_max_bytes`): the
    fn set_translate_cap(&self, _cap: usize) {}

    /// Clone this writer as a trait object.
    fn clone_box(&self) -> Box<dyn ProtocolWriter>;
}

/// Per-stream, ingress-keyed framing state for the shared [`StreamTranslate`], the vtable seam that
pub trait StreamFraming: Send {
    /// Egress-chunk seam (OpenAI ingress), called for every reframed `chat.completion.chunk` just
    fn on_egress_chunk(&mut self, _chunk: &mut serde_json::Value) -> Option<serde_json::Value> {
        None
    }

    /// Combined-stop-delta seam (Bedrock ingress), called on a combined
    fn on_combined_stop_delta(
        &mut self,
        _stop_reason: crate::codec::ir::IrStopReason,
        _stop_sequence: Option<String>,
        _usage: &crate::codec::ir::IrUsage,
    ) -> Option<Vec<crate::codec::ir::IrStreamEvent>> {
        None
    }

    /// Usage-only-delta seam (Bedrock ingress), called for a trailing `MessageDelta{stop_reason:
    fn on_usage_only_delta(&mut self) -> Option<bool> {
        None
    }

    /// Finish seam (Bedrock ingress), called once at end of stream. `Some(event)` when a `metadata`
    fn on_finish(&mut self) -> Option<crate::codec::ir::IrStreamEvent> {
        None
    }

    /// Metadata-metrics seam (Bedrock ingress), called in the eventstream branch for each frame with
    fn inject_streaming_metrics(
        &self,
        _event_type: &str,
        _data: &mut serde_json::Value,
        _elapsed_ms: Option<u64>,
    ) {
    }

    /// Stream-abort seam (eventstream ingress): the error type name this ingress emits as the
    fn abort_exception_type(&self) -> Option<&'static str> {
        None
    }

    /// Terminal-usage-fold seam (plain SSE ingresses: Anthropic/Gemini/Cohere/Responses). These carry
    fn folds_terminal_usage(&self) -> bool {
        true
    }

    /// Client-intent seam (OpenAI ingress): whether the original client request had
    fn set_client_include_usage(&mut self, _include: bool) {}

    /// Same-protocol edit seam (OpenAI ingress). The same-protocol path re-emits each upstream frame
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

/// Stable dense wire indices for blocks this writer actually emits. Closed mappings remain for
/// late citations and cloned stream writers. Isolated event callers that supplied no message start
/// retain their historical input indices; a real message starts dense allocation at zero.
#[derive(Clone)]
pub(crate) struct EmittedBlockIndices {
    indices: std::collections::BTreeMap<usize, usize>,
    finished: bool,
    started: bool,
}

impl EmittedBlockIndices {
    pub(crate) const fn new() -> Self {
        Self {
            indices: std::collections::BTreeMap::new(),
            finished: true,
            started: false,
        }
    }

    /// A duplicate start in an active stream retains its mappings and identity.
    pub(crate) fn begin(&mut self) -> bool {
        if self.started && !self.finished {
            return false;
        }
        self.indices.clear();
        self.finished = false;
        self.started = true;
        true
    }

    pub(crate) fn open(&mut self, index: usize) -> usize {
        let next = if self.started {
            self.indices.len()
        } else {
            index
        };
        *self.indices.entry(index).or_insert(next)
    }

    pub(crate) fn wire(&self, index: usize) -> usize {
        self.indices.get(&index).copied().unwrap_or(index)
    }

    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }
}

/// Inert default [`StreamFraming`] for every protocol whose ingress stream has no per-stream
pub struct PassthroughFraming;

impl StreamFraming for PassthroughFraming {}

/// Bundled Protocol with name + reader + writer.
pub struct Protocol {
    name: &'static str,

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
    pub fn name_static(&self) -> &'static str {
        self.name
    }

    /// This protocol's declaration (the constant facts core reads by field). Always `Some` for a
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
pub fn protocol_for(name: &str) -> Option<Protocol> {
    entry_of(name).map(|d| (d.protocol)())
}

/// One dialect's registration: everything this plane resolves by name, declared in the dialect's
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
pub fn entry_of(name: &str) -> Option<&'static DialectEntry> {
    crate::codec::DIALECTS
        .iter()
        .copied()
        .find(|d| d.decl.name == name)
}

/// Run `f` against the named dialect's writer built on the stack, with no `Protocol` and no `Box`.
pub fn with_writer<T>(name: &str, f: impl FnOnce(&dyn ProtocolWriter) -> T) -> Option<T> {
    let entry = entry_of(name)?;
    let mut f = Some(f);
    let mut out = None;
    (entry.with_writer)(&mut |w| out = f.take().map(|f| f(w)));
    out
}

/// The reader twin of [`with_writer`]: the named dialect's reader on the stack (readers are unit
pub fn with_reader<T>(name: &str, f: impl FnOnce(&dyn ProtocolReader) -> T) -> Option<T> {
    let entry = entry_of(name)?;
    let mut f = Some(f);
    let mut out = None;
    (entry.with_reader)(&mut |r| out = f.take().map(|f| f(r)));
    out
}

/// The plane-local equivalent of the former core `handlers::protocol_error` round trip: the shared
pub fn protocol_error(
    name: &str,
    status: u16,
    body: &[u8],
) -> busbar_contract::upstream::RawUpstreamError {
    protocol_error_with_head(name, status, &[], body)
}

/// The same protocol-local extraction with borrowed response heads, consumed synchronously.
pub fn protocol_error_with_head(
    name: &str,
    status: u16,
    head: busbar_contract::protocol::HeadFields<'_>,
    body: &[u8],
) -> busbar_contract::upstream::RawUpstreamError {
    with_reader(name, |r| {
        r.extract_error_with_head(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            head,
            body,
        )
    })
    .unwrap_or_else(|| busbar_contract::upstream::RawUpstreamError::from_status(status))
}

pub struct DialectRef(&'static str);

/// Construct the neutral [`DialectCodec`] facade for a named dialect, `const` so each dialect's
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
pub fn native_tool_id_prefix(protocol_name: &str) -> Option<&'static str> {
    crate::codec::decl_of(protocol_name).and_then(|d| d.native_tool_id_prefix)
}

/// Marker segment in a busbar-minted tool id, so the request-side reverse can tell a reshaped id from
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
pub const TOOL_ID_REMAP_MAX_MEMO: usize = crate::codec::dialect::MAX_OPEN_TOOL_CALLS;

impl ToolIdRemap {
    /// Reshape one egress tool id into the ingress protocol's native form (deterministic, memoized).
    pub fn native_for(&mut self, ingress_protocol: &str, egress_id: &str) -> String {
        let Some(prefix) = native_tool_id_prefix(ingress_protocol) else {
            return egress_id.to_string();
        };
        if let Some(existing) = self.map.get(egress_id) {
            return existing.clone();
        }
        let native = format!("{prefix}{TOOL_ID_REMAP_MARKER}{}", ::hex::encode(egress_id));

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
pub fn decode_native_tool_id(ingress_protocol: &str, id: &str) -> Option<String> {
    let prefix = native_tool_id_prefix(ingress_protocol)?;
    let rest = id.strip_prefix(prefix)?;
    let hexpart = rest.strip_prefix(TOOL_ID_REMAP_MARKER)?;

    if hexpart.is_empty() {
        return None;
    }

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
