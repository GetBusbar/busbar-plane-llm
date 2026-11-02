// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The cross-dialect translate pipeline (#83a SD-3: dialect machinery). Translating one dialect's

use crate::codec::egress_prep::EgressPrep;
use busbar_contract::billing::Billing;
use busbar_contract::codec::{
    CodecError, EgressWire, IngressReject, OperationHandler, TranslatedResponse,
};
use busbar_contract::ir::facts::IrFacts;
use serde_json::Value;
use std::collections::BTreeSet;

/// What a request-translation hop reads from: a parsed JSON object `Value` (the value-codec fast

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

pub struct TranslatedRequest {
    pub wire: EgressWire,
    pub dropped_controls: Vec<String>,
}

/// Why a cross-protocol request translation could not proceed: three terminal outcomes. The seam

pub enum TranslateReqReject {
    /// The ingress reader refused the body; the caller renders `ingress_reject_response`.
    Ingress(IngressReject),
    /// The egress protocol does not serve this operation; the caller renders the 404
    EgressUnsupported,
    /// The egress dialect cannot represent the request without silent loss; the caller renders a 400
    Unrepresentable(String),
}

/// What a non-stream response-translation hop reads from: the upstream 2xx body, a parsed JSON

pub enum TranslateRespInput<'a> {
    Json(&'a Value),
    Opaque(&'a [u8]),
}

/// The single neutral translate entrypoint on the codec cell (G6 step 4). Every request/response hop

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
    /// `prep` is this plane's resolved preparation context; `model` is the resolved lane wire model.
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
                ir.prepare_for_egress(prep)
                    .map_err(|CodecError::Malformed(reason)| {
                        TranslateReqReject::Unrepresentable(reason)
                    })?;

                let egress_proto = egress_proto.ok_or(TranslateReqReject::EgressUnsupported)?;
                ir.egress_representable(egress_proto)
                    .map_err(TranslateReqReject::Unrepresentable)?;

                let bytes = ir.write_egress_request_bytes(egress_proto, model);

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
                let seam = crate::codec::drops::Seam {
                    direction: crate::codec::drops::Direction::Request,
                    ingress: prep.ingress_protocol,
                    egress: egress_proto.unwrap_or_default(),
                };
                let (written, dropped) = crate::codec::drops::scope(seam, || {
                    let mut ir = self
                        .read_request_value(v)
                        .map_err(TranslateReqReject::Ingress)?;
                    ir.prepare_for_egress(prep)
                        .map_err(|CodecError::Malformed(reason)| {
                            TranslateReqReject::Unrepresentable(reason)
                        })?;

                    crate::codec::proto_codec::with_reader(prep.ingress_protocol, |r| {
                        crate::codec::drops::note_unmodelled_blocks(
                            r.grammar().request_blocks,
                            v,
                            crate::codec::drops::UNMODELLED_REQUEST_BLOCK,
                        )
                    });

                    let egress_proto = egress_proto.ok_or(TranslateReqReject::EgressUnsupported)?;
                    if let Err(reason) = ir.egress_representable(egress_proto) {
                        return Err(TranslateReqReject::Unrepresentable(reason));
                    }
                    let controls = ir.egress_dropped_controls(egress_proto);

                    let wire = ir.write_egress_request(host, egress_proto, model);

                    if let EgressWire::Unrepresentable { reason } = wire {
                        return Err(TranslateReqReject::Unrepresentable(reason));
                    }
                    Ok((wire, controls))
                });
                let (wire, controls) = written?;

                let dropped_controls = merge_dropped_paths(
                    controls.into_iter().map(|control| {
                        crate::codec::drops::wire_path(prep.ingress_protocol, control, &[])
                    }),
                    dropped,
                );
                Ok(TranslatedRequest {
                    wire,
                    dropped_controls,
                })
            }
        }
    }

    /// Read the request facts the hook seam / lazy-body projects from: the one read through this

    fn read_facts_value(&self, v: &Value) -> Result<Box<dyn IrFacts + Send + Sync>, IngressReject> {
        Ok(self.read_request_value(v)?.facts())
    }

    /// Byte-codec sibling of [`Self::read_facts_value`], for an opaque/multipart body whose caller

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

        ingress_serves_op: bool,
        ingress_protocol: &str,

        lane_model: &str,
        now: u64,
        wants_stream: bool,
        elapsed_ms: Option<u64>,

        ingress_request_body: Option<&Value>,

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

                if wants_stream {
                    if let Some(frames) =
                        ir.wrap_buffered_as_stream(host, ingress_protocol, elapsed_ms)
                    {
                        return Ok((usage, TranslatedResponse::StreamFrames(frames)));
                    }
                }

                Ok((
                    usage,
                    ir.write_ingress_response(host, ingress_protocol, ingress_serves_op),
                ))
            }
        }
    }
}

/// Keep controls first and ordinary drops in encounter order. A value-qualified earlier path

fn merge_dropped_paths(
    controls: impl IntoIterator<Item = String>,
    dropped: Vec<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut named = BTreeSet::new();
    for path in controls {
        if named.insert(path.clone()) {
            out.push(path);
        }
    }
    for path in dropped {
        let prefix = format!("{path}=");

        let covered = named.contains(&path)
            || named
                .range(prefix.clone()..)
                .next()
                .is_some_and(|prior| prior.starts_with(&prefix));
        if !covered {
            named.insert(path.clone());
            out.push(path);
        }
    }
    out
}

impl<T: OperationHandler + ?Sized> TranslateCodec for T {}

#[cfg(test)]
#[path = "tests/egress_wire_tests.rs"]
mod egress_wire_tests;

#[cfg(test)]
#[path = "tests/drop_merge_tests.rs"]
mod drop_merge_tests;
