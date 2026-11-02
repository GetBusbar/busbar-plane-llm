// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The six leaf-op `IrHandle`s: embeddings / image / rerank / moderation / transcription / speech.
//! Each wraps its concrete `crate::codec::ir::*` request/response and writes itself onto the peer
//! dialect by protocol string via the `(op, protocol)` `leaf_codec` dispatchers. The prep arms are
//! no-ops (leaf ops carry no cross-protocol reshape); `set_model` stamps the model; billing projects
//! each op's own usage. The write methods JSON-parse the leaf bytes, else fall through to the opaque
//! `Bytes`/`Typed` form, byte-identical to the dissolved `IrReq`/`IrResp` arms.

use crate::codec::ir::audio::{SpeechReq, SpeechResp, TranscriptionReq, TranscriptionResp};
use crate::codec::ir::embeddings::{EmbeddingsReq, EmbeddingsResp};
use crate::codec::ir::image::{ImageReq, ImageResp};
use crate::codec::ir::moderation::{ModerationReq, ModerationResp};
use crate::codec::ir::rerank::{RerankReq, RerankResp};
use busbar_contract::billing::Billing;
use busbar_contract::codec::{EgressWire, TranslatedResponse};
use busbar_contract::ir::facts::IrFacts;
use busbar_contract::ir::handle::sealed::Sealed;
use busbar_contract::ir::handle::IrHandle;
use busbar_contract::operation::OpVerb;
use busbar_contract::SlabBytes;
use bytes::Bytes;
use serde_json::Value;

/// A leaf dialect writer's request body as the contract's byte slab the handle hands the engine.
fn slab(body: Bytes) -> SlabBytes {
    SlabBytes::from(&body[..])
}

/// The refusal a transcription writer's empty write becomes: a request always carries its model, so
/// a writer that wrote nothing refused it (invalid audio or a form delimiter in its bytes). Refused at
/// the seam, never sent upstream as an empty body.
fn unwritable(egress_proto: &str) -> String {
    format!(
        "this request cannot be written onto {egress_proto}: its audio cannot be represented in \
         the form it would be sent in"
    )
}

/// One leaf operation's request and response handles over its `leaf_codec` writers. The request
/// writes the egress dialect's JSON (else, with the model stamped, opaque bytes); the response writes
/// the ingress dialect's answer (JSON, else the typed bytes).
macro_rules! leaf_handles {
    ($req:ident($req_ir:ty), $resp:ident($resp_ir:ty), $verb:ident, $write_req:ident, $write_resp:ident,
     stream: $stream:tt, req_billing: $req_billing:tt, resp_billing: $resp_billing:tt, refuse_empty: $refuse:tt, refuse_empty_response: $resp_refuse:tt) => {
        pub struct $req(pub $req_ir);
        pub struct $resp(pub $resp_ir);
        impl Sealed for $req {}
        impl Sealed for $resp {}

        impl IrHandle for $req {
            fn verb(&self) -> OpVerb {
                OpVerb::$verb
            }
            leaf_handles!(@stream $stream);
            leaf_handles!(@billing $req_billing);
            leaf_handles!(@representable $req);
            fn facts(&self) -> Box<dyn IrFacts + Send + Sync> {
                Box::new(self.0.clone())
            }
            fn set_model(&mut self, model: &str) {
                self.0.model = model.to_string();
            }
            fn write_egress_request(
                &mut self,
                _host: &dyn busbar_contract::codec::CodecHost,
                egress_proto: &str,
                model: &str,
            ) -> EgressWire {
                leaf_handles!(@format $req, self.0, egress_proto);
                let bytes = super::leaf_codec::$write_req(egress_proto, &self.0);
                leaf_handles!(@refuse $refuse bytes egress_proto);
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) => EgressWire::Json(v),
                    Err(_) => {
                        self.0.model = model.to_string();
                        EgressWire::Bytes(slab(super::leaf_codec::$write_req(egress_proto, &self.0)))
                    }
                }
            }
            fn write_egress_request_bytes(&mut self, egress_proto: &str, model: &str) -> SlabBytes {
                leaf_handles!(@format_bytes $req, self.0, egress_proto);
                self.0.model = model.to_string();
                slab(super::leaf_codec::$write_req(egress_proto, &self.0))
            }
        }

        impl IrHandle for $resp {
            fn verb(&self) -> OpVerb {
                OpVerb::$verb
            }
            leaf_handles!(@billing $resp_billing);
            leaf_handles!(@response_echo $resp);
            fn write_ingress_response(
                &self,
                _host: &dyn busbar_contract::codec::CodecHost,
                ingress_proto: &str,
                serves_op: bool,
            ) -> TranslatedResponse {
                if !serves_op {
                    return TranslatedResponse::IngressUnsupported;
                }
                let wb = super::leaf_codec::$write_resp(ingress_proto, &self.0);
                leaf_handles!(@refuse_response $resp_refuse wb);
                if wb.content_type.as_bytes() != busbar_contract::protocol::APPLICATION_JSON.as_bytes() {
                    return TranslatedResponse::Typed(wb);
                }
                match serde_json::from_slice::<Value>(&wb.bytes) {
                    Ok(v) => TranslatedResponse::Json(v),
                    Err(_) => TranslatedResponse::Typed(wb),
                }
            }
            fn write_ingress_response_bytes(
                &self,
                _host: &dyn busbar_contract::codec::CodecHost,
                ingress_proto: &str,
                serves_op: bool,
            ) -> TranslatedResponse {
                if !serves_op {
                    return TranslatedResponse::Untranslatable;
                }
                let wb = super::leaf_codec::$write_resp(ingress_proto, &self.0);
                leaf_handles!(@refuse_response $resp_refuse wb);
                TranslatedResponse::Typed(wb)
            }
        }
    };
    (@stream none) => {
        fn wants_stream(&self) -> bool {
            false
        }
    };
    (@response_echo TranscriptionRespHandle) => {
        fn apply_request_echo(&mut self, request: &Value) {
            if let Some(format) = request.get("response_format").and_then(Value::as_str) {
                self.0.response_format = Some(format.to_string());
            }
        }
    };
    (@response_echo $other:ident) => {};
    (@representable TranscriptionReqHandle) => {
        fn egress_representable(&self, target: &str) -> Result<(), String> {
            transcription_format_refusal(&self.0, target).map_or(Ok(()), Err)
        }
    };
    (@representable $other:ident) => {};
    (@format TranscriptionReqHandle, $ir:expr, $proto:ident) => {
        if let Some(reason) = transcription_format_refusal(&$ir, $proto) {
            return EgressWire::Unrepresentable { reason };
        }
    };
    (@format $other:ident, $ir:expr, $proto:ident) => {};
    (@format_bytes TranscriptionReqHandle, $ir:expr, $proto:ident) => {
        if transcription_format_refusal(&$ir, $proto).is_some() {
            return SlabBytes::default();
        }
    };
    (@format_bytes $other:ident, $ir:expr, $proto:ident) => {};
    (@stream field) => {
        fn wants_stream(&self) -> bool {
            self.0.stream
        }
    };
    (@billing none) => {};
    (@billing own) => {
        fn billing(&self) -> Option<Billing> {
            self.0.billing()
        }
    };
    (@billing flat) => {
        fn billing(&self) -> Option<Billing> {
            Some(Billing::Flat)
        }
    };
    (@refuse no $bytes:ident $proto:ident) => {};
    (@refuse_response no $wire:ident) => {};
    (@refuse_response yes $wire:ident) => {
        if $wire.bytes.is_empty() {
            return TranslatedResponse::Untranslatable;
        }
    };
    (@refuse yes $bytes:ident $proto:ident) => {
        if $bytes.is_empty() {
            return EgressWire::Unrepresentable {
                reason: unwritable($proto),
            };
        }
    };
}

/// A plain foreign transcript has no timing information from which an SRT/VTT document can be
/// reconstructed. Refuse that cross-target request before work is sent; native relay is unchanged.
fn transcription_format_refusal(request: &TranscriptionReq, target: &str) -> Option<String> {
    let format = request.response_format.as_deref()?;
    (target != crate::codec::proto_codec::PROTO_OPENAI && matches!(format, "srt" | "vtt"))
        .then(|| format!("transcription response_format '{format}' cannot be represented by {target}: timestamped output is unavailable"))
}

leaf_handles!(EmbeddingsReqHandle(EmbeddingsReq), EmbeddingsRespHandle(EmbeddingsResp), EMBEDDINGS,
    embeddings_write_request, embeddings_write_response,
    stream: none, req_billing: none, resp_billing: own, refuse_empty: no, refuse_empty_response: no);
leaf_handles!(ImageReqHandle(ImageReq), ImageRespHandle(ImageResp), IMAGE,
    image_write_request, image_write_response,
    stream: none, req_billing: none, resp_billing: own, refuse_empty: no, refuse_empty_response: no);
leaf_handles!(RerankReqHandle(RerankReq), RerankRespHandle(RerankResp), RERANK,
    rerank_write_request, rerank_write_response,
    stream: none, req_billing: none, resp_billing: own, refuse_empty: no, refuse_empty_response: no);
leaf_handles!(ModerationReqHandle(ModerationReq), ModerationRespHandle(ModerationResp), MODERATION,
    moderation_write_request, moderation_write_response,
    stream: none, req_billing: none, resp_billing: flat, refuse_empty: no, refuse_empty_response: no);
leaf_handles!(TranscriptionReqHandle(TranscriptionReq), TranscriptionRespHandle(TranscriptionResp), TRANSCRIPTION,
    transcription_write_request, transcription_write_response,
    stream: field, req_billing: none, resp_billing: own, refuse_empty: yes, refuse_empty_response: no);
// Speech meters at the request seam (`SpeechReq::billing`: the input's exact character count),
// knowable here and never from the opaque audio answer.
leaf_handles!(SpeechReqHandle(SpeechReq), SpeechRespHandle(SpeechResp), SPEECH,
    speech_write_request, speech_write_response,
    stream: field, req_billing: own, resp_billing: own, refuse_empty: no, refuse_empty_response: yes);
