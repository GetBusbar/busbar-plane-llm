// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The cross-dialect translate pipeline (#83a SD-3: dialect machinery). Translating one dialect's
//! request or response into another's (read through the ingress cell, prepare, write through the
//! egress dialect) is this plane's own rule (Law 5), so it lives with the dialects: the
//! [`TranslateCodec`](crate::codec::translate::TranslateCodec) entrypoint, blanket-implemented for
//! every codec cell, and the values its two directions read from and answer with. The cell traits are
//! contract shapes (`busbar_contract::codec`); nothing here names the host.

use busbar_contract::billing::Billing;
use busbar_contract::codec::{
    CodecError, EgressWire, IngressReject, OperationHandler, TranslatedResponse,
};
use busbar_contract::ir::egress_prep::EgressPrep;
use busbar_contract::ir::facts::IrFacts;
use serde_json::Value;

/// What a request-translation hop reads from: a parsed JSON object `Value` (the value-codec fast
/// path chat overrides to avoid a re-serialize), or opaque bytes plus content-type (a
/// multipart/binary wire the byte codec owns). [`TranslateCodec`] branches on this.
pub enum TranslateReqInput<'a> {
    /// A JSON-object body, routed through the value codecs.
    Json(&'a Value),
    /// An opaque/binary body (multipart transcription, audio speech), routed through the byte codecs.
    Opaque {
        bytes: &'a [u8],
        content_type: &'a str,
    },
}

/// The result of a cross-protocol request translation: the egress wire plus what did not cross (the
/// controls the egress dialect dropped, then every other dropped member by wire path in
/// `codec::drops`) for the seam's audit-and-allow event. Empty on the opaque path.
pub struct TranslatedRequest {
    pub wire: EgressWire,
    pub dropped_controls: Vec<String>,
}

/// Why a cross-protocol request translation could not proceed: three terminal outcomes. The seam
/// owns the projection into an ingress-native error, so the entrypoint names no HTTP shape.
pub enum TranslateReqReject {
    /// The ingress reader refused the body; the caller renders `ingress_reject_response`.
    Ingress(IngressReject),
    /// The egress protocol does not serve this operation; the caller renders the 404
    /// (`DETAIL_MODEL_UNSUPPORTED_OPERATION`). Raised only after read+prepare, so a malformed body
    /// still rejects as a 400, not a 404.
    EgressUnsupported,
    /// The egress dialect cannot represent the request without silent loss; the caller renders a 400
    /// carrying `reason`.
    Unrepresentable(String),
}

/// What a non-stream response-translation hop reads from: the upstream 2xx body, a parsed JSON
/// `Value` (value-codec path) or opaque bytes (a non-JSON wire such as speech audio). The engine
/// parses the body once and branches into these.
pub enum TranslateRespInput<'a> {
    Json(&'a Value),
    Opaque(&'a [u8]),
}

/// The single neutral translate entrypoint on the codec cell (G6 step 4). Every request/response hop
/// flows through it so core never orchestrates the concrete read, prepare, write pipeline:
/// `wire.rs`'s request seam calls [`Self::translate_request`], the engine's non-stream response arm
/// calls [`Self::translate_response`], and the hook / lazy-body side calls [`Self::read_facts`] /
/// [`Self::read_facts_value`]. Streaming routes through the registry
/// `proto::new_stream_translator_on` factory (G6 step 3).
///
/// It wraps the concrete `IrReq`/`IrResp` pipeline unchanged: the defaults read/prepare/write
/// through the same `OperationHandler` methods the old call sites named inline, so every hop stays
/// byte-identical. The internals are deliberately concrete here.
///
/// Blanket-implemented for every [`OperationHandler`], so any codec cell is a `TranslateCodec`
/// with no per-cell wiring.
pub trait TranslateCodec: OperationHandler {
    /// Cross-protocol request translation: `self` is the ingress codec (it reads), `egress` the
    /// lane's codec (it writes).
    ///   - Opaque: read, `prepare_for_egress`, `set_model`, egress `write_request` (bytes). No
    ///     representability guard or dropped controls (an opaque body has none); `egress` is
    ///     resolved and 404-checked by the caller, so it is always `Some` here.
    ///   - JSON: read, `prepare_for_egress`, (egress absent gives [`TranslateReqReject::EgressUnsupported`]),
    ///     representability guard, collect dropped controls, egress `write_request_value` (`Some` is a
    ///     JSON body the router still post-shapes; `None` is `set_model` + `write_request` bytes).
    ///
    /// `prep` is the router-built neutral param bag; `model` is the resolved lane wire model.
    fn translate_request(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        input: TranslateReqInput<'_>,
        egress_proto: Option<&str>,
        prep: &EgressPrep,
        model: &str,
    ) -> Result<TranslatedRequest, TranslateReqReject> {
        match input {
            TranslateReqInput::Opaque {
                bytes,
                content_type,
            } => {
                let mut ir = self
                    .read_request(bytes, content_type)
                    .map_err(TranslateReqReject::Ingress)?;
                ir.prepare_for_egress(prep);
                // The opaque caller resolves and 404-checks `egress`, so it is always `Some`; the guard avoids a panic.
                let egress_proto = egress_proto.ok_or(TranslateReqReject::EgressUnsupported)?;
                // The handle writes itself onto the egress dialect (by protocol string) after `set_model`.
                let bytes = ir.write_egress_request_bytes(egress_proto, model);
                // A write that produced nothing is a refusal, never an empty body forwarded upstream
                // (`IrHandle::write_egress_request`'s contract).
                if bytes.is_empty() {
                    return Err(TranslateReqReject::Unrepresentable(format!(
                        "this request cannot be written onto {egress_proto}"
                    )));
                }
                Ok(TranslatedRequest {
                    wire: EgressWire::Bytes(bytes),
                    dropped_controls: Vec::new(),
                })
            }
            TranslateReqInput::Json(v) => {
                // One translate attempt: every drop (reader, seam, writer) goes through the one drop
                // path and returns here for the audit.
                let seam = crate::codec::drops::Seam {
                    direction: crate::codec::drops::Direction::Request,
                    ingress: prep.ingress_protocol,
                    egress: egress_proto.unwrap_or_default(),
                };
                let (written, dropped) = crate::codec::drops::scope(seam, || {
                    let mut ir = self
                        .read_request_value(v)
                        .map_err(TranslateReqReject::Ingress)?;
                    ir.prepare_for_egress(prep);
                    // Content blocks the caller's dialect has and its reader does not model.
                    crate::codec::proto_codec::with_reader(prep.ingress_protocol, |r| {
                        crate::codec::drops::note_unmodelled_blocks(
                            r.grammar().request_blocks,
                            v,
                            crate::codec::drops::UNMODELLED_REQUEST_BLOCK,
                        )
                    });
                    // Egress-absent surfaces only after read+prepare, so a malformed body still
                    // rejects as a 400 (via `Ingress`), not a 404.
                    let egress_proto = egress_proto.ok_or(TranslateReqReject::EgressUnsupported)?;
                    if let Err(reason) = ir.egress_representable(egress_proto) {
                        return Err(TranslateReqReject::Unrepresentable(reason));
                    }
                    let controls = ir.egress_dropped_controls(egress_proto);
                    // The handle owns the value-first / set-model+bytes write onto the egress dialect.
                    let wire = ir.write_egress_request(host, egress_proto, model);
                    // A write that could not be represented is a refusal on the same terminal as the
                    // pre-write representability guard: the guard answers for what it can see before
                    // the write, this for what only the writer can.
                    if let EgressWire::Unrepresentable { reason } = wire {
                        return Err(TranslateReqReject::Unrepresentable(reason));
                    }
                    Ok((wire, controls))
                });
                let (wire, controls) = written?;
                // A control is named by the caller's wire path for it, as every other drop is.
                let mut dropped_controls: Vec<String> = Vec::new();
                for control in controls {
                    let path = crate::codec::drops::wire_path(prep.ingress_protocol, control, &[]);
                    if !dropped_controls.contains(&path) {
                        dropped_controls.push(path);
                    }
                }
                // A writer's drop of a member a control already names by its value
                // (`tool_choice=none` for `tool_choice`) is that control's drop, recorded once.
                for path in dropped {
                    let named = dropped_controls.iter().any(|c| {
                        c == &path
                            || c.strip_prefix(path.as_str())
                                .is_some_and(|rest| rest.starts_with('='))
                    });
                    if !named {
                        dropped_controls.push(path);
                    }
                }
                Ok(TranslatedRequest {
                    wire,
                    dropped_controls,
                })
            }
        }
    }

    /// Read the request facts the hook seam / lazy-body projects from: the one read through this
    /// codec's reader, projected to [`IrFacts`]. The value-codec path (chat calls its proto reader
    /// directly, no re-serialize on the hot path).
    fn read_facts_value(&self, v: &Value) -> Result<Box<dyn IrFacts + Send + Sync>, IngressReject> {
        Ok(self.read_request_value(v)?.facts())
    }

    /// Byte-codec sibling of [`Self::read_facts_value`], for an opaque/multipart body whose caller
    /// text is reachable only through the byte reader.
    fn read_facts(
        &self,
        wire: &[u8],
        content_type: &str,
    ) -> Result<Box<dyn IrFacts + Send + Sync>, IngressReject> {
        Ok(self.read_request(wire, content_type)?.facts())
    }

    /// Cross-protocol non-stream response translation: `self` is the egress codec (it reads the
    /// upstream 2xx body), `ingress_op`/`ingress_writer` write the caller's dialect.
    ///   - read (`read_response` / `read_response_value`, `Err` is [`CodecError`]), capture the
    ///     billable usage from the pre-`prepare_for_ingress` IR, fill the serving model from
    ///     `lane_model` iff the upstream body carried none
    ///     ([`busbar_contract::ir::handle::IrHandle::fill_response_model_if_absent`]), then
    ///     `prepare_for_ingress`;
    ///   - JSON, wants-stream: `wrap_buffered_as_stream` `Some` gives [`TranslatedResponse::StreamFrames`];
    ///   - JSON: ingress absent gives [`TranslatedResponse::IngressUnsupported`]; else
    ///     `write_response_value` (`Some` is [`TranslatedResponse::Json`], `None`
    ///     [`TranslatedResponse::Typed`]);
    ///   - Opaque: ingress present gives [`TranslatedResponse::Typed`], absent
    ///     [`TranslatedResponse::Untranslatable`].
    ///
    /// The returned usage is always the read IR's (`ir.usage()`), which the caller bills before
    /// rendering, so a read-succeeded-but-undelivered terminal (404 / 500) still bills. The caller
    /// keeps telemetry, the untranslatable-metadata warn, billing, budget accounting, response-metrics
    /// injection, the gemini-array wrap and all response building.
    #[allow(clippy::too_many_arguments)]
    fn translate_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        input: TranslateRespInput<'_>,
        // Whether the caller's ingress protocol serves this operation (resolved by the engine via `op_for`).
        ingress_serves_op: bool,
        ingress_protocol: &str,
        // The resolved lane wire model this hop routed to, filling the response model when the
        // upstream carried none ([`busbar_contract::ir::handle::IrHandle::fill_response_model_if_absent`]).
        // Filled after read (the upstream's own model wins) and before `prepare_for_ingress` and the
        // ingress write, so a cross-protocol response reports the real serving model.
        lane_model: &str,
        now: u64,
        wants_stream: bool,
        elapsed_ms: Option<u64>,
        // The original ingress request body, when parsed (the cross-protocol path always has it; see
        // `Hop::body`), threaded to `IrHandle::apply_request_echo` so a dialect whose response spec
        // mirrors request members (OpenAI Responses) answers with the client's values, not defaults.
        // `None` for the opaque/audio bridge.
        ingress_request_body: Option<&Value>,
        // The translation cap the plane enforces, threaded to `IrHandle::apply_translate_cap` so a
        // writer re-emitting the response as a stream bounds what it accumulates.
        translate_cap: usize,
    ) -> Result<(Option<Billing>, TranslatedResponse), CodecError> {
        match input {
            TranslateRespInput::Opaque(bytes) => {
                let mut ir = self.read_response(host, bytes)?;
                let usage = ir.billing();
                ir.fill_response_model_if_absent(lane_model);
                if let Some(body) = ingress_request_body {
                    ir.apply_request_echo(body);
                }
                ir.prepare_for_ingress(ingress_protocol, now);
                // The handle writes itself onto the ingress dialect: present is Typed, absent is
                // Untranslatable, by `ingress_protocol` + `ingress_serves_op`.
                Ok((
                    usage,
                    ir.write_ingress_response_bytes(host, ingress_protocol, ingress_serves_op),
                ))
            }
            TranslateRespInput::Json(v) => {
                let mut ir = self.read_response_value(host, v)?;
                let usage = ir.billing();
                ir.fill_response_model_if_absent(lane_model);
                if let Some(body) = ingress_request_body {
                    ir.apply_request_echo(body);
                }
                ir.apply_translate_cap(translate_cap);
                ir.prepare_for_ingress(ingress_protocol, now);
                // Buffered-2xx-to-native-stream synthesis (a wants-stream ingress served a non-SSE
                // upstream): tried first; `None` falls through to the normal write.
                if wants_stream {
                    if let Some(frames) =
                        ir.wrap_buffered_as_stream(host, ingress_protocol, elapsed_ms)
                    {
                        return Ok((usage, TranslatedResponse::StreamFrames(frames)));
                    }
                }
                // The handle owns the absent => IngressUnsupported / value-first => Json / else Typed(WireBody) write.
                Ok((
                    usage,
                    ir.write_ingress_response(host, ingress_protocol, ingress_serves_op),
                ))
            }
        }
    }
}

impl<T: OperationHandler + ?Sized> TranslateCodec for T {}

#[cfg(test)]
#[path = "tests/egress_wire_tests.rs"]
mod egress_wire_tests;
