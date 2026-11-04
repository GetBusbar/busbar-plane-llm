// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Bedrock `RequestHandler` + cells. Embeddings first (Titan, via InvokeModel).

use crate::codec::cells::{CodecError, IngressReject, RequestHandler};
use crate::codec::cells::{EgressCtx, WireBody};
use crate::codec::ir::embeddings::{
    EmbInput, EmbeddingItem, EmbeddingsReq, EmbeddingsResp, EncFmt, VectorData,
};
use crate::codec::keys;
use crate::codec::leaf_codec::LeafCodec;
use busbar_contract::operation::OpVerb;
use busbar_contract::SlabBytes;
use bytes::Bytes;
use serde_json::{json, Value};

pub struct BedrockRequestHandler;
/// This protocol's own chat instance; remove it and the registry arm and chat 404s via the
/// standard no-handler path.
static CHAT: super::super::chat_handle::ChatOperation =
    super::super::chat_handle::ChatOperation(super::VENDOR_NAME);
static EMB: BedrockEmbeddings = BedrockEmbeddings;
static IMG: BedrockImage = BedrockImage;
static RERANK: BedrockRerank = BedrockRerank;

/// Bedrock's row of the support matrix, as data. A verb absent from it is a genuine gap (the
/// standard no-handler 404).
static CELLS: &[crate::codec::cells::Cell] = &[
    (OpVerb::CHAT, &CHAT),
    (OpVerb::EMBEDDINGS, &EMB),
    (OpVerb::IMAGE, &IMG),
    (OpVerb::RERANK, &RERANK),
];

impl RequestHandler for BedrockRequestHandler {
    dialect_identity!(super::VENDOR_NAME);
    fn upstream_path(&self, ctx: &EgressCtx) -> String {
        if ctx.operation == OpVerb::CHAT {
            let verb = if ctx.stream {
                "converse-stream"
            } else {
                "converse"
            };
            return format!("/model/{}/{verb}", ctx.model);
        }

        format!("/model/{}/invoke", ctx.model)
    }
    fn resolve_operation(&self, path: &str, body: &[u8]) -> Option<OpVerb> {
        if path.ends_with("/converse") || path.ends_with("/converse-stream") {
            return Some(OpVerb::CHAT);
        }
        if path.ends_with("/invoke") {
            let has = |n: &[u8]| body.windows(n.len()).any(|w| w == n);

            if has(b"\"query\"") && has(b"\"documents\"") {
                return Some(OpVerb::RERANK);
            }
            if has(b"\"textToImageParams\"") {
                return Some(OpVerb::IMAGE);
            }
            if has(b"\"inputText\"") {
                return Some(OpVerb::EMBEDDINGS);
            }
        }
        None
    }
    fn path_model(&self, path: &str) -> Option<String> {
        let rest = path.strip_prefix("/model/")?;
        let (model, _verb) = rest.rsplit_once('/')?;
        (!model.is_empty()).then(|| model.to_string())
    }
}

/// Amazon Titan Image Generator via `/model/{id}/invoke`: prompt in, `images[]` (b64) out. The model
/// rides the path; routing fills the IR's `model` (`IrReq::set_model`).
struct BedrockImage;

leaf_op! {
    BedrockImage: super::VENDOR_NAME,
    ImageReqHandle = read_image_request,
    ImageRespHandle = read_image_response;
    // Buffer the same-protocol non-stream 2xx body so the default `extract_usage` runs the op's
    // reader and bills once. Titan/SDXL are per-image with no token usage, so the tap bills 0 tokens
    // and the per-image cost basis on the cross-protocol seam carries the charge.
    fn taps_usage(&self) -> bool {
        true
    }
}

/// A request or answer member the InvokeModel writers do not send, dropped with a warn (the one
/// drop path, `codec::drops`).
fn invoke_drop(name: &'static str) {
    crate::codec::drops::writer_drop!(
        crate::codec::drops::member(name),
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        [member = name,],
        "Bedrock InvokeModel: dropping `{name}`, a member v1.5.5 never sent on this operation \
         (it is not carried)"
    );
}

/// IR to Titan image request wire ([`BedrockImage::write_request`]): `taskType`, prompt `text` and
/// `numberOfImages` only.
pub fn write_image_request(r: &crate::codec::ir::image::ImageReq) -> Bytes {
    let body = json!({
        "taskType": "TEXT_IMAGE",
        (super::TEXT_TO_IMAGE_PARAMS): { (keys::TEXT): r.prompt.clone().unwrap_or_default() },
        (super::IMAGE_GENERATION_CONFIG): { (super::NUMBER_OF_IMAGES): r.n.unwrap_or(1) },
    });
    for (name, present) in [
        ("negative_prompt", r.negative_prompt.is_some()),
        (keys::SEED, r.seed.is_some()),
        ("guidance_scale", r.guidance_scale.is_some()),
        ("size", r.size.is_some()),
        (keys::QUALITY, r.quality.is_some()),
    ] {
        if present {
            invoke_drop(name);
        }
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Titan image response wire ([`BedrockImage::write_response`]).
pub fn write_image_response(r: &crate::codec::ir::image::ImageResp) -> WireBody {
    for image in &r.images {
        if image.b64.is_none() && image.url.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::member("ir.images[].url"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [path = "ir.images[].url",],
                "Bedrock image response: dropping ir.images[].url because Titan requires base64; \
                 native image source metadata is unavailable"
            );
        }
    }
    let images: Vec<&str> = r.images.iter().filter_map(|i| i.b64.as_deref()).collect();
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&json!({ (super::IMAGES): images })).unwrap_or_default(),
    ))
}

/// Amazon Titan Embeddings via `/model/{id}/invoke`: `inputText` (+ v2 dims/normalize). The model
/// rides the path; routing fills it via `IrReq::set_model`.
struct BedrockEmbeddings;

leaf_op! {
    BedrockEmbeddings: super::VENDOR_NAME,
    EmbeddingsReqHandle = read_embeddings_request,
    EmbeddingsRespHandle = read_embeddings_response;
    // Token-metered: buffer the same-protocol non-stream 2xx body so the default `extract_usage`
    // reads `usage` and bills the key's TPM/spend.
    fn taps_usage(&self) -> bool {
        true
    }
    fn extract_usage(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        _ingress: &str,
        body: &[u8],
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, crate::codec::cells::UsageTapFault> {
        let value: Value = crate::codec::json::parse(body)
            .map_err(|_| crate::codec::cells::UsageTapFault::Decode)?;
        Ok(read_embeddings_usage(&value))
    }
    fn recover_response_billing(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &[u8],
    ) -> Option<busbar_contract::billing::Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        read_embeddings_usage(&value).map(busbar_contract::billing::Billing::Tokens)
    }

}

/// IR to Titan embeddings request wire ([`BedrockEmbeddings::write_request`]).
pub fn write_embeddings_request(r: &EmbeddingsReq) -> Bytes {
    let text = match &r.input {
        EmbInput::Text(v) => {
            if v.len() > 1 {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::member(keys::INPUT),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [dropped = v.len() - 1,],
                    "Titan embeddings takes one input; embedding only the first of a \
                     multi-input request (the rest are not sent)"
                );
            }
            v.first().cloned().unwrap_or_default()
        }
        other => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::member(keys::INPUT),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [dropped = 1,],
                "Titan embeddings takes text input only; dropping a non-text embeddings \
                 input ({other:?} kind) with no analog"
            );
            String::new()
        }
    };
    let mut body = json!({ (super::INPUT_TEXT): text });
    if let Some(d) = r.dimensions {
        body[keys::DIMENSIONS] = json!(d);
    }

    if r.normalize.is_some() {
        invoke_drop(super::NORMALIZE);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Titan embeddings response wire ([`BedrockEmbeddings::write_response`]).
pub fn write_embeddings_response(r: &EmbeddingsResp) -> WireBody {
    let floats: Vec<f32> = r
        .embeddings
        .first()
        .and_then(|item| match item.vectors.get(&EncFmt::Float) {
            Some(VectorData::Float(v)) => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut body = json!({ (keys::EMBEDDING): floats });
    if let Some(u) = &r.usage {
        body[super::INPUT_TEXT_TOKEN_COUNT] = json!(u.input);
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Bedrock rerank models (`cohere.rerank-*`, `amazon.rerank-*`) via `/model/{id}/invoke`:
struct BedrockRerank;

leaf_op! {
    BedrockRerank: super::VENDOR_NAME,
    RerankReqHandle = read_rerank_request,
    RerankRespHandle = read_rerank_response;
    // Search-unit metered like the Cohere cell: buffer the same-protocol non-stream 2xx body so the
    // tap reads the `meta.billed_units.search_units` it billed.
    fn taps_usage(&self) -> bool {
        true
    }
    fn extract_usage(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        _ingress: &str,
        body: &[u8],
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, crate::codec::cells::UsageTapFault>
    {
        crate::codec::rerank_wire::extract_usage(body, super::VENDOR_NAME)
    }
    fn recover_response_billing(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &[u8],
    ) -> Option<busbar_contract::billing::Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        crate::codec::ir::rerank::RerankResp {
            search_units: crate::codec::rerank_wire::read_search_units(&value, super::VENDOR_NAME),
            ..Default::default()
        }.billing()
    }

}

/// IR to rerank request wire ([`BedrockRerank::write_request`]).
pub fn write_rerank_request(r: &crate::codec::ir::rerank::RerankReq) -> Bytes {
    let mut body = json!({
        (keys::QUERY): r.query,
        (keys::DOCUMENTS): r.documents,
        "api_version": 2,
    });
    if let Some(n) = r.top_n {
        body[keys::TOP_N] = json!(n);
    }

    if r.return_documents.is_some() {
        invoke_drop(keys::RETURN_DOCUMENTS);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to rerank response wire ([`BedrockRerank::write_response`]).
pub fn write_rerank_response(r: &crate::codec::ir::rerank::RerankResp) -> WireBody {
    if r.results.iter().any(|x| x.document.is_some()) {
        invoke_drop(keys::DOCUMENT);
    }
    let results: Vec<Value> = r
        .results
        .iter()
        .map(|x| json!({"index": x.index, "relevance_score": x.relevance_score}))
        .collect();
    let mut body = json!({ (keys::RESULTS): results });
    if let Some(id) = &r.id {
        body[keys::ID] = json!(id);
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "tests/handler_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/titan_roundtrip_regression_tests.rs"]
mod titan_roundtrip_regression_tests;

/// Wire to concrete `ImageReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch so
/// neither needs a downcast.
pub fn read_image_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::image::ImageReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let params = wire
        .get(super::TEXT_TO_IMAGE_PARAMS)
        .cloned()
        .unwrap_or_default();
    let cfg = wire
        .get(super::IMAGE_GENERATION_CONFIG)
        .cloned()
        .unwrap_or_default();
    Ok(crate::codec::ir::image::ImageReq {
        prompt: params
            .get(keys::TEXT)
            .and_then(Value::as_str)
            .map(str::to_string),
        negative_prompt: params
            .get(super::NEGATIVE_TEXT)
            .and_then(Value::as_str)
            .map(str::to_string),
        n: cfg
            .get(super::NUMBER_OF_IMAGES)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        seed: cfg.get(keys::SEED).and_then(Value::as_u64),
        guidance_scale: cfg
            .get(super::CFG_SCALE)
            .and_then(Value::as_f64)
            .map(|f| {
                let value = f as f32;
                value.is_finite().then_some(value).ok_or_else(|| {
                    IngressReject::BadRequest("guidance scale exceeds the finite f32 range".into())
                })
            })
            .transpose()?,

        size: match (
            cfg.get(super::WIDTH).and_then(Value::as_u64),
            cfg.get(super::HEIGHT).and_then(Value::as_u64),
        ) {
            (Some(w), Some(h)) => match (u32::try_from(w).ok(), u32::try_from(h).ok()) {
                (Some(width), Some(height)) => {
                    Some(crate::codec::ir::image::ImageSize::Wh { width, height })
                }
                _ => None,
            },
            _ => None,
        },
        quality: cfg
            .get(keys::QUALITY)
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    })
}

/// Wire to concrete `ImageResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_image_response(wire: &[u8]) -> Result<crate::codec::ir::image::ImageResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let images = v
        .get(super::IMAGES)
        .and_then(Value::as_array)
        .ok_or_else(|| CodecError::Malformed("image response requires an images array".into()))?
        .iter()
        .map(|value| {
            let b64 = value.as_str().ok_or_else(|| {
                CodecError::Malformed("image response contains a non-string image".into())
            })?;
            Ok(crate::codec::framework::ImageOutput {
                b64: Some(b64.to_string()),
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, CodecError>>()?;

    let cost_basis = Some(crate::codec::ir::image::CostBasis {
        count: u32::try_from(images.len()).unwrap_or(u32::MAX),
        ..Default::default()
    });
    Ok(crate::codec::ir::image::ImageResp {
        images,
        cost_basis,
        ..Default::default()
    })
}

/// Wire to concrete `EmbeddingsReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::embeddings::EmbeddingsReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let Some(text) = wire.get(super::INPUT_TEXT).and_then(Value::as_str) else {
        return Err(IngressReject::BadRequest(
            "invoke embeddings requires `inputText`".into(),
        ));
    };
    Ok(crate::codec::ir::embeddings::EmbeddingsReq {
        input: EmbInput::Text(vec![text.to_string()]),
        dimensions: wire
            .get(keys::DIMENSIONS)
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok()),
        normalize: wire.get(super::NORMALIZE).and_then(Value::as_bool),
        encoding_formats: vec![EncFmt::Float],
        ..Default::default()
    })
}

/// Wire to concrete `EmbeddingsResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_response(
    wire: &[u8],
) -> Result<crate::codec::ir::embeddings::EmbeddingsResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let mut item = EmbeddingItem::default();
    {
        let f = v
            .get(keys::EMBEDDING)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CodecError::Malformed("embedding response requires an embedding array".into())
            })?;
        let values = f
            .iter()
            .map(|value| {
                let number = value.as_f64().ok_or_else(|| {
                    CodecError::Malformed("embedding contains a nonnumeric component".into())
                })?;
                let number = number as f32;
                if !number.is_finite() {
                    return Err(CodecError::Malformed(
                        "embedding component exceeds the finite f32 range".into(),
                    ));
                }
                Ok(number)
            })
            .collect::<Result<Vec<_>, CodecError>>()?;
        item.vectors
            .insert(EncFmt::Float, VectorData::Float(values));
    }

    let usage = read_embeddings_usage(&v);
    Ok(EmbeddingsResp {
        embeddings: vec![item],
        usage,
        ..Default::default()
    })
}

fn read_embeddings_usage(v: &Value) -> Option<busbar_contract::billing::TokenUsage> {
    crate::codec::usage_count::billed_count_opt(Some(v), super::INPUT_TEXT_TOKEN_COUNT)
        .unwrap_or_else(|unreadable| {
            crate::codec::usage_count::refused_count(super::VENDOR_NAME, &unreadable);
            None
        })
        .map(|n| busbar_contract::billing::TokenUsage {
            input: n,
            ..Default::default()
        })
}

/// Wire to concrete `RerankReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_rerank_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::rerank::RerankReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let query = wire
        .get(keys::QUERY)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let documents = crate::codec::rerank_wire::read_documents(wire.get(keys::DOCUMENTS))
        .map_err(|message| IngressReject::BadRequest(message.into()))?;
    if query.is_empty() || documents.is_empty() {
        return Err(IngressReject::BadRequest(
            "rerank request requires `query` and `documents`".into(),
        ));
    }
    Ok(crate::codec::ir::rerank::RerankReq {
        model: String::new(),
        query,
        documents,
        top_n: wire
            .get(keys::TOP_N)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),

        return_documents: wire.get(keys::RETURN_DOCUMENTS).and_then(Value::as_bool),
        ..Default::default()
    })
}

/// Wire to concrete `RerankResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_rerank_response(
    wire: &[u8],
) -> Result<crate::codec::ir::rerank::RerankResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    Ok(crate::codec::ir::rerank::RerankResp {
        id: v.get(keys::ID).and_then(Value::as_str).map(str::to_string),
        results: crate::codec::rerank_wire::read_results(v.get(keys::RESULTS))
            .map_err(|message| CodecError::Malformed(message.into()))?,

        search_units: crate::codec::rerank_wire::read_search_units(&v, super::VENDOR_NAME),
        ..Default::default()
    })
}

/// This dialect's row of the leaf-op `(operation, protocol)` dispatch, carried on `super::ENTRY`.
pub(crate) const LEAF: crate::codec::leaf_codec::LeafCodecs =
    crate::codec::leaf_codec::LeafCodecs {
        embeddings: Some(LeafCodec {
            write_request: write_embeddings_request,
            write_response: write_embeddings_response,
            read_request: read_embeddings_request,
            read_response: read_embeddings_response,
        }),
        rerank: Some(LeafCodec {
            write_request: write_rerank_request,
            write_response: write_rerank_response,
            read_request: read_rerank_request,
            read_response: read_rerank_response,
        }),
        image: Some(LeafCodec {
            write_request: write_image_request,
            write_response: write_image_response,
            read_request: read_image_request,
            read_response: read_image_response,
        }),
        ..crate::codec::leaf_codec::LeafCodecs::NONE
    };

#[cfg(test)]
#[path = "tests/audit_embedding_billing_tests.rs"]
mod audit_embedding_billing_tests;
