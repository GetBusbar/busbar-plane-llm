// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Cohere `RequestHandler` + cells. Embeddings via `/v2/embed`.

use super::{
    BINARY, EMBEDDINGS, EMBEDDING_TYPES, FLOAT, INPUT_TYPE, INT8, MAX_TOKENS_PER_DOC,
    OUTPUT_DIMENSION, TEXTS, TRUNCATE, UBINARY, UINT8, VENDOR_NAME,
};
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

/// Endpoint paths, on both the egress side (`upstream_path`) and the ingress match
/// (`resolve_operation`), single-sourced so they cannot drift.
const PATH_CHAT: &str = "/v2/chat";
const PATH_EMBED: &str = "/v2/embed";
const PATH_RERANK: &str = "/v2/rerank";

pub struct CohereRequestHandler;
/// This protocol's own chat instance; remove it and the registry arm and chat 404s via the
/// standard no-handler path.
static CHAT: super::super::chat_handle::ChatOperation =
    super::super::chat_handle::ChatOperation(VENDOR_NAME);
static EMB: CohereEmbeddings = CohereEmbeddings;
static RERANK: CohereRerank = CohereRerank;

/// Cohere's row of the support matrix, as data. A verb absent from it is the standard no-handler
/// 404: Cohere has no moderation/image/audio surface, and the protocol-surface verbs are MCP's and A2A's.
static CELLS: &[crate::codec::cells::Cell] = &[
    (OpVerb::CHAT, &CHAT),
    (OpVerb::EMBEDDINGS, &EMB),
    (OpVerb::RERANK, &RERANK),
];

/// The egress half of the path constants above; `resolve_operation` reads the same ones inbound.
static PATHS: &[(OpVerb, &str)] = &[
    (OpVerb::CHAT, PATH_CHAT),
    (OpVerb::RERANK, PATH_RERANK),
    (OpVerb::EMBEDDINGS, PATH_EMBED),
];

impl RequestHandler for CohereRequestHandler {
    dialect_identity!(VENDOR_NAME);
    fn upstream_path(&self, ctx: &EgressCtx) -> String {
        crate::codec::framework::path_of(PATHS, ctx.operation)
            .unwrap_or(PATH_EMBED)
            .into()
    }
    fn resolve_operation(&self, path: &str, _body: &[u8]) -> Option<OpVerb> {
        if path.ends_with(PATH_CHAT) {
            Some(OpVerb::CHAT)
        } else if path.ends_with(PATH_EMBED) {
            Some(OpVerb::EMBEDDINGS)
        } else if path.ends_with(PATH_RERANK) {
            Some(OpVerb::RERANK)
        } else {
            None
        }
    }
}

/// Cohere `embedding_types` name for an IR encoding, 1:1 with Cohere v2's six, so a requested
/// encoding is served natively, not downgraded to float.
fn cohere_embedding_type(f: &EncFmt) -> &'static str {
    match f {
        EncFmt::Float => FLOAT,
        EncFmt::Base64 => keys::BASE64,
        EncFmt::Int8 => INT8,
        EncFmt::Uint8 => UINT8,
        EncFmt::Binary => BINARY,
        EncFmt::Ubinary => UBINARY,
    }
}

/// Every IR encoding Cohere's `/v2/embed` supports, for iterating the response keys.
const ALL_ENCODINGS: [EncFmt; 6] = [
    EncFmt::Float,
    EncFmt::Base64,
    EncFmt::Int8,
    EncFmt::Uint8,
    EncFmt::Binary,
    EncFmt::Ubinary,
];

/// Inverse of [`cohere_embedding_type`]: the full 1:1 map (not only base64), so an `int8`/`uint8`/
/// `binary`/`ubinary` ask is not collapsed to float on read.
fn cohere_encoding_format(s: &str) -> EncFmt {
    match s {
        keys::BASE64 => EncFmt::Base64,
        INT8 => EncFmt::Int8,
        UINT8 => EncFmt::Uint8,
        BINARY => EncFmt::Binary,
        UBINARY => EncFmt::Ubinary,
        _ => EncFmt::Float,
    }
}

/// Cohere v2 embeddings (`/v2/embed`). `input_type` is required (default: a document role). As
/// ingress: `texts[]` + required `input_type`.
struct CohereEmbeddings;

leaf_op! {
    CohereEmbeddings: super::VENDOR_NAME,
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
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, crate::codec::cells::UsageTapFault>
    {
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

/// IR to Cohere v2 embed request wire ([`CohereEmbeddings::write_request`]).
pub fn write_embeddings_request(r: &EmbeddingsReq) -> Bytes {
    let texts = match &r.input {
        EmbInput::Text(v) => v.clone(),
        other => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::member(keys::INPUT),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [dropped = 1,],
                "Cohere embeddings input is text-only here; dropping a non-text embeddings \
                 input ({other:?} kind) with no analog"
            );
            Vec::new()
        }
    };
    let input_type = r
        .input_type
        .clone()
        .unwrap_or_else(|| "search_document".to_string());

    let embedding_types: Vec<&str> = if r.encoding_formats.is_empty() {
        vec![FLOAT]
    } else {
        r.encoding_formats
            .iter()
            .map(cohere_embedding_type)
            .collect()
    };
    let mut body = json!({
        (keys::MODEL): r.model,
        (TEXTS): texts,
        (INPUT_TYPE): input_type,
        (EMBEDDING_TYPES): embedding_types,
    });

    if let Some(d) = r.dimensions {
        body[OUTPUT_DIMENSION] = json!(d);
    }
    if let Some(t) = &r.truncate {
        body[TRUNCATE] = json!(t);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Cohere v2 embed response wire ([`CohereEmbeddings::write_response`]).
pub fn write_embeddings_response(r: &EmbeddingsResp) -> WireBody {
    let mut by_enc: std::collections::BTreeMap<EncFmt, Vec<(usize, Value)>> =
        std::collections::BTreeMap::new();
    for item in &r.embeddings {
        if item.vectors.is_empty() {
            return WireBody::json(SlabBytes::default());
        }
        for (enc, vd) in &item.vectors {
            let cell = match vd {
                VectorData::Float(v) => json!(v),
                VectorData::Base64(s) => json!(s),
                VectorData::Int(v) => json!(v),
            };
            by_enc.entry(*enc).or_default().push((item.index, cell));
        }
    }
    let mut emb = serde_json::Map::new();
    let count = by_enc.values().next().map_or(0, Vec::len);
    for (enc, vals) in &mut by_enc {
        vals.sort_unstable_by_key(|(index, _)| *index);
        if vals.len() != count
            || vals
                .iter()
                .enumerate()
                .any(|(position, (index, _))| *index != position)
        {
            return WireBody::json(SlabBytes::default());
        }
        let vectors: Vec<&Value> = vals.iter().map(|(_, vector)| vector).collect();
        emb.insert(cohere_embedding_type(enc).to_string(), json!(vectors));
    }

    if emb.is_empty() {
        emb.insert(FLOAT.to_string(), json!(Vec::<Vec<f32>>::new()));
    }
    let mut body = json!({
        "response_type": "embeddings_by_type",
        (EMBEDDINGS): emb,
    });
    if let Some(id) = &r.id {
        body[keys::ID] = json!(id);
    }
    if let Some(texts) = &r.input_echo {
        body[TEXTS] = json!(texts);
    }
    if let Some(u) = &r.usage {
        body[keys::META] = json!({ (keys::BILLED_UNITS): { (keys::INPUT_TOKENS): crate::codec::usage_count::wire_token_count(u, "input", u.input) } });
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Cohere v2 rerank (`/v2/rerank`): `{model, query, documents[], top_n?}` to `{results: [{index,
/// relevance_score}], meta.billed_units.search_units}`.
struct CohereRerank;

leaf_op! {
    CohereRerank: super::VENDOR_NAME,
    RerankReqHandle = read_rerank_request,
    RerankRespHandle = read_rerank_response;
    // Search-unit metered: buffer the same-protocol non-stream 2xx body so the tap reads
    // `meta.billed_units.search_units` onto both books, as the cross-protocol path does;
    // a verbatim relay kept no copy, so the units reached neither.
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
            exact_usage: crate::codec::rerank_wire::exact_usage(&value, super::VENDOR_NAME),
            ..Default::default()
        }.billing()
    }

}

/// IR to Cohere v2 rerank request wire ([`CohereRerank::write_request`]).
pub fn write_rerank_request(r: &crate::codec::ir::rerank::RerankReq) -> Bytes {
    let mut body = json!({
        (keys::MODEL): r.model,
        (keys::QUERY): r.query,
        (keys::DOCUMENTS): r.documents,
    });
    if let Some(n) = r.top_n {
        body[keys::TOP_N] = json!(n);
    }
    if let Some(m) = r.max_tokens_per_doc {
        body[MAX_TOKENS_PER_DOC] = json!(m);
    }

    if let Some(rd) = r.return_documents {
        body[keys::RETURN_DOCUMENTS] = json!(rd);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Cohere v2 rerank response wire ([`CohereRerank::write_response`]).
pub fn write_rerank_response(r: &crate::codec::ir::rerank::RerankResp) -> WireBody {
    let results: Vec<Value> = r
        .results
        .iter()
        .map(|x| {
            let mut o = json!({(keys::INDEX): x.index, (keys::RELEVANCE_SCORE): x.relevance_score});

            if let Some(doc) = &x.document {
                o[keys::DOCUMENT] = json!({ (keys::TEXT): doc });
            }
            o
        })
        .collect();
    let mut body = json!({ (keys::RESULTS): results });
    if let Some(id) = &r.id {
        body[keys::ID] = json!(id);
    }
    if let Some(exact) = r
        .exact_usage
        .as_ref()
        .and_then(|u| u.exact_counts.as_ref())
        .and_then(|e| e.get(crate::codec::ir::rerank::SEARCH_UNITS_CLASS))
    {
        body[keys::META] = serde_json::json!({(keys::BILLED_UNITS): {(keys::SEARCH_UNITS): exact}});
    } else if let Some(su) = r.search_units {
        body[keys::META] = json!({ (keys::BILLED_UNITS): { (keys::SEARCH_UNITS): su } });
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "tests/rerank_tests.rs"]
mod rerank_tests;

/// Wire to concrete `EmbeddingsReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::embeddings::EmbeddingsReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let texts = wire
        .get(TEXTS)
        .and_then(Value::as_array)
        .ok_or_else(|| IngressReject::BadRequest("embed request requires `texts`".into()))?
        .iter()
        .map(|text| {
            text.as_str().map(str::to_string).ok_or_else(|| {
                IngressReject::BadRequest("embed texts entries must be strings".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if texts.is_empty() {
        return Err(IngressReject::BadRequest(
            "embed request requires `texts`".into(),
        ));
    }
    let encoding_formats = wire
        .get(EMBEDDING_TYPES)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(cohere_encoding_format)
                .collect()
        })
        .unwrap_or_else(|| vec![EncFmt::Float]);
    Ok(crate::codec::ir::embeddings::EmbeddingsReq {
        model: wire
            .get(keys::MODEL)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input: EmbInput::Text(texts),
        input_type: wire
            .get(INPUT_TYPE)
            .and_then(Value::as_str)
            .map(str::to_string),
        dimensions: wire
            .get(OUTPUT_DIMENSION)
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok()),
        truncate: wire
            .get(TRUNCATE)
            .and_then(Value::as_str)
            .map(str::to_string),
        encoding_formats,
        ..Default::default()
    })
}

fn read_embeddings_usage(value: &Value) -> Option<busbar_contract::billing::TokenUsage> {
    crate::codec::usage_count::token_usage(
        super::VENDOR_NAME,
        value
            .get(keys::META)
            .and_then(|m| m.get(keys::BILLED_UNITS)),
        &[(keys::INPUT_TOKENS, busbar_contract::records::UNIT_INPUT)],
    )
}

/// Wire to concrete `EmbeddingsResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_response(
    wire: &[u8],
) -> Result<crate::codec::ir::embeddings::EmbeddingsResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;

    let malformed = || CodecError::Malformed("invalid Cohere embedding vector".into());
    let emb = v
        .get(EMBEDDINGS)
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let arrays: Vec<(EncFmt, &Vec<Value>)> = ALL_ENCODINGS
        .iter()
        .filter_map(|&e| emb.get(cohere_embedding_type(&e)).map(|value| (e, value)))
        .map(|(encoding, value)| {
            value
                .as_array()
                .map(|array| (encoding, array))
                .ok_or_else(malformed)
        })
        .collect::<Result<_, _>>()?;
    if arrays.is_empty() {
        return Err(malformed());
    }
    let count = arrays.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let embeddings: Vec<EmbeddingItem> = (0..count)
        .map(|idx| {
            let mut item = EmbeddingItem {
                index: idx,
                ..Default::default()
            };
            for (enc, arr) in &arrays {
                let cell = arr.get(idx).ok_or_else(malformed)?;
                let vd = match enc {
                    EncFmt::Float => VectorData::Float(
                        cell.as_array()
                            .ok_or_else(malformed)?
                            .iter()
                            .map(|value| {
                                let number = value.as_f64().ok_or_else(malformed)? as f32;
                                number.is_finite().then_some(number).ok_or_else(malformed)
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    EncFmt::Base64 => {
                        VectorData::Base64(cell.as_str().ok_or_else(malformed)?.to_string())
                    }

                    _ => VectorData::Int(
                        cell.as_array()
                            .ok_or_else(malformed)?
                            .iter()
                            .map(|value| {
                                value
                                    .as_i64()
                                    .and_then(|number| i32::try_from(number).ok())
                                    .ok_or_else(malformed)
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                };
                item.vectors.insert(*enc, vd);
            }
            Ok(item)
        })
        .collect::<Result<_, CodecError>>()?;

    let usage = read_embeddings_usage(&v);
    Ok(EmbeddingsResp {
        id: v.get(keys::ID).and_then(Value::as_str).map(str::to_string),
        embeddings,
        usage,
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
        model: wire
            .get(keys::MODEL)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        query,
        documents,
        top_n: wire
            .get(keys::TOP_N)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        max_tokens_per_doc: wire
            .get(MAX_TOKENS_PER_DOC)
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
        exact_usage: crate::codec::rerank_wire::exact_usage(&v, super::VENDOR_NAME),
        ..Default::default()
    })
}

/// This dialect's row of the leaf-op `(operation, protocol)` dispatch, carried on `super::ENTRY`.
pub(crate) const LEAF: crate::codec::leaf_codec::LeafCodecs =
    crate::codec::leaf_codec::LeafCodecs {
        embeddings: Some(LeafCodec {
            write_request: write_embeddings_request,
            write_response: write_embeddings_response,
            #[cfg(test)]
            read_request: read_embeddings_request,
            #[cfg(test)]
            read_response: read_embeddings_response,
        }),
        rerank: Some(LeafCodec {
            write_request: write_rerank_request,
            write_response: write_rerank_response,
            #[cfg(test)]
            read_request: read_rerank_request,
            #[cfg(test)]
            read_response: read_rerank_response,
        }),
        ..crate::codec::leaf_codec::LeafCodecs::NONE
    };

#[cfg(test)]
#[path = "tests/embedding_shape_billing_tests.rs"]
mod embedding_shape_billing_tests;
