// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! LLM operation codec cells and request handlers.

use busbar_contract::billing::{Billing, TokenUsage};
pub use busbar_contract::codec::{
    CodecError, CodecHost, EgressWire, TranslatedResponse, UsageTapFault, WireBody,
    TRANSLATE_BODY_MAX_BYTES_DEFAULT,
};
use busbar_contract::ir::handle::IrHandle;
use busbar_contract::operation::OpVerb;
use serde_json::Value;

/// What routing hands a request handler so it can render the upstream URL path. RESOLVED PRIMITIVES
pub struct EgressCtx<'a> {
    /// Which operation's endpoint to render — the template selector.
    pub operation: OpVerb,
    /// The resolved wire model id (routing calls `Lane::wire_model()`), for protocols that carry the
    pub model: &'a str,
    /// Whether the caller asked to stream (chat/audio path variants); `false` for the JSON ops.
    pub stream: bool,
    /// Optional per-provider path-BASE override (the lane's `path_base`). For URL-model protocols it
    pub path_base: Option<&'a str>,
}

/// A request that could not be parsed into this operation's IR — rendered as a caller-dialect 4xx
/// (via the existing `proxy::ingress_error`). `UnsupportedSubOp` is the second 404 site
/// (`ImageIr.op` unsupported for the model) — distinct from handler-absence, same terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressReject {
    BadRequest(String),
    UnsupportedSubOp { op: OpVerb, model: String },
}

/// ONE ROW OF A PROTOCOL'S SUPPORT MATRIX — a verb the protocol speaks and the codec that speaks it.
pub type Cell = (OpVerb, &'static dyn OperationHandler);

/// THE ROW LOOKUP every [`RequestHandler::operation_handler`] is — stated once so there are not
pub fn cell_of(cells: &'static [Cell], op: OpVerb) -> Option<&'static dyn OperationHandler> {
    cells
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, handler)| *handler)
}

/// THE (verb → upstream path) LOOKUP, for the protocols whose egress paths are constants rather
pub fn path_of(paths: &'static [(OpVerb, &'static str)], op: OpVerb) -> Option<&'static str> {
    paths
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, path)| *path)
}

/// A pure per-(protocol × operation) codec. Feed it wire, assert the IR; feed it IR, assert the wire.
pub trait OperationHandler: Send + Sync {
    /// WHAT THIS UPSTREAM'S FAILURE MEANT — the attributed outcome of one outbound attempt, which
    fn extract_error(
        &self,
        status: u16,
        _body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        busbar_contract::upstream::RawUpstreamError::from_status(status)
    }

    /// Can this operation produce a client-facing incremental stream?
    fn streaming(&self) -> bool {
        false
    }
    /// Should the non-stream 2xx body be buffered so [`Self::extract_usage`] can read it?
    fn taps_usage(&self) -> bool {
        false
    }
    /// The caller's stream intent, from the parsed ingress body. Chat reads the body's top-level
    fn wants_stream(&self, _body: &Value) -> bool {
        false
    }
    /// A body-derived session-affinity key (used only when no affinity header is present). Chat uses
    fn body_affinity_key<'a>(&self, _body: &'a Value) -> Option<&'a str> {
        None
    }
    /// Extract billable usage from a complete same-protocol non-stream 2xx body (called once at stream
    fn extract_usage(
        &self,
        host: &dyn CodecHost,
        _ingress_protocol: &str,
        body: &[u8],
    ) -> Result<Option<TokenUsage>, UsageTapFault> {
        match self.read_response(host, body) {
            Ok(r) => Ok(r.token_usage()),
            Err(_) => Err(UsageTapFault::Decode),
        }
    }
    /// Recover independently reported billing when the complete response cannot be decoded.
    fn recover_response_billing(&self, _host: &dyn CodecHost, _body: &[u8]) -> Option<Billing> {
        None
    }
    /// The Content-Type of THIS operation's egress request wire (what `write_request` emits).
    fn egress_request_content_type(&self) -> &'static str {
        crate::codec::protocol::APPLICATION_JSON
    }

    /// The egress `Accept` header for the upstream request. `egress_stream_accept` is the egress
    fn egress_accept(
        &self,
        egress_stream_accept: &'static str,
        wants_stream: bool,
    ) -> &'static str {
        if wants_stream {
            egress_stream_accept
        } else {
            crate::codec::protocol::APPLICATION_JSON
        }
    }

    /// Value-level codec bridge (request) — for engine seams that already hold a PARSED JSON body
    fn read_request_value(&self, v: &Value) -> Result<Box<dyn IrHandle>, IngressReject> {
        let bytes = serde_json::to_vec(v).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
        self.read_request(&bytes, crate::codec::protocol::APPLICATION_JSON)
    }
    /// Value-level codec bridge (response).
    fn read_response_value(
        &self,
        host: &dyn CodecHost,
        v: &Value,
    ) -> Result<Box<dyn IrHandle>, CodecError> {
        let bytes = serde_json::to_vec(v).map_err(|e| CodecError::Malformed(e.to_string()))?;
        self.read_response(host, &bytes)
    }

    /// Wire → IR HANDLE (request). The OperationHandler owns the ENTIRE wire format: it receives RAW
    fn read_request(
        &self,
        body: &[u8],
        content_type: &str,
    ) -> Result<Box<dyn IrHandle>, IngressReject>;
    /// Egress wire → IR HANDLE (response) — for the usage tap or a cross-protocol translation. Raw
    fn read_response(
        &self,
        host: &dyn CodecHost,
        wire: &[u8],
    ) -> Result<Box<dyn IrHandle>, CodecError>;
}

/// A protocol's dialect + its OperationHandlers (one impl per protocol).
pub trait RequestHandler: Send + Sync {
    /// Stable protocol identity (matches the dialect's own protocol name). Called only from `busbar-core`'s
    fn protocol_name(&self) -> &'static str;

    /// This protocol's row of the support matrix. `None` ⇒ the protocol does not serve the operation
    fn operation_handler(&self, op: OpVerb) -> Option<&dyn OperationHandler>;

    /// WHICH operation this request asks for — the RequestHandler knows its protocol and reads the
    fn resolve_operation(&self, path: &str, body: &[u8]) -> Option<OpVerb>;

    /// The model named in the PATH, for path-model dialects (`models/{m}:action`,
    fn path_model(&self, _path: &str) -> Option<String> {
        None
    }

    /// The `(protocol, operation) → path template` map: this protocol's upstream URL for the operation
    fn upstream_path(&self, ctx: &EgressCtx) -> String;
}
