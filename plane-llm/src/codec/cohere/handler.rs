// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Cohere `RequestHandler` + cells. Embeddings via `/v2/embed`.

use super::{
    BINARY, EMBEDDINGS, EMBEDDING_TYPES, FLOAT, INPUT_TYPE, INT8, MAX_TOKENS_PER_DOC,
    OUTPUT_DIMENSION, TEXTS, TRUNCATE, UBINARY, UINT8, VENDOR_NAME,
};
use crate::codec::ir::embeddings::{
    EmbInput, EmbeddingItem, EmbeddingsReq, EmbeddingsResp, EncFmt, VectorData,
};
use crate::codec::keys;
use crate::codec::leaf_codec::LeafCodec;
use busbar_contract::codec::{CodecError, IngressReject, RequestHandler};
use busbar_contract::codec::{EgressCtx, WireBody};
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
static CELLS: &[busbar_contract::codec::Cell] = &[
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
        // Unreachable: `operation_handler` returns `None` for a verb absent from the table. The fallback is the pre-1.6.0 answer.
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
/// `binary`/`ubinary` ask is not collapsed to float on read. Unknown strings fall back to float.
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
    // Honor the requested encoding(s): `embedding_types` map 1:1 to the IR `EncFmt`, so base64 is
    // served natively, not downgraded to float. Default float.
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
    // Carry the reader's shape/truncation controls (Cohere was the lone embeddings writer dropping
    // them): `output_dimension` (Matryoshka on embed-v4) and `truncate`.
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
    // Emit every encoding the IR carries under its own Cohere key (`float`, `base64`, `int8`, ...)
    // so e.g. an int8 response from a cross-protocol backend is not dropped: the write twin of the
    // six-encoding read. `embeddings_by_type` holds multiple per-item arrays in candidate order.
    let mut by_enc: std::collections::BTreeMap<EncFmt, Vec<Value>> =
        std::collections::BTreeMap::new();
    for item in &r.embeddings {
        for (enc, vd) in &item.vectors {
            let cell = match vd {
                VectorData::Float(v) => json!(v),
                VectorData::Base64(s) => json!(s),
                VectorData::Int(v) => json!(v),
            };
            by_enc.entry(*enc).or_default().push(cell);
        }
    }
    let mut emb = serde_json::Map::new();
    for (enc, vals) in &by_enc {
        emb.insert(cohere_embedding_type(enc).to_string(), json!(vals));
    }
    // An empty `float` key when the IR carried no vectors, for response-shape stability.
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
        body[keys::META] = json!({ (keys::BILLED_UNITS): { (keys::INPUT_TOKENS): u.input } });
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Cohere v2 rerank (`/v2/rerank`): `{model, query, documents[], top_n?}` to `{results: [{index,
/// relevance_score}], meta.billed_units.search_units}`. Documents are bare strings or `{text}`
/// objects, normalized to strings.
struct CohereRerank;

leaf_op! {
    CohereRerank: super::VENDOR_NAME,
    RerankReqHandle = read_rerank_request,
    RerankRespHandle = read_rerank_response;
    // Search-unit metered: buffer the same-protocol non-stream 2xx body so the tap reads
    // `meta.billed_units.search_units` onto both books, as the cross-protocol path does (item 134);
    // a verbatim relay kept no copy, so the units reached neither.
    fn taps_usage(&self) -> bool {
        true
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
    // Carry `return_documents`: Cohere echoes each ranked document's text when set; dropping it ignored the caller's ask.
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
            // Echo the ranked document in Cohere's `{text}` shape when requested.
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
    if let Some(su) = r.search_units {
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
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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

/// Wire to concrete `EmbeddingsResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_response(
    wire: &[u8],
) -> Result<crate::codec::ir::embeddings::EmbeddingsResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    // Cohere returns each requested encoding under its own key (`embeddings.float`, `.base64`,
    // `.int8`, ...), positionally aligned. Read every encoding the request leg can ask for, so an
    // int8/uint8/binary/ubinary response is not dropped (`float` is absent when only that was
    // requested). Float to Float, base64 to Base64, the int forms to Int.
    let emb = v.get(EMBEDDINGS);
    let arrays: Vec<(EncFmt, &Vec<Value>)> = ALL_ENCODINGS
        .iter()
        .filter_map(|&e| {
            emb.and_then(|o| o.get(cohere_embedding_type(&e)))
                .and_then(Value::as_array)
                .map(|a| (e, a))
        })
        .collect();
    let count = arrays.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let embeddings: Vec<EmbeddingItem> = (0..count)
        .map(|idx| {
            let mut item = EmbeddingItem {
                index: idx,
                ..Default::default()
            };
            for (enc, arr) in &arrays {
                let Some(cell) = arr.get(idx) else { continue };
                let vd = match enc {
                    EncFmt::Float => cell.as_array().map(|f| {
                        VectorData::Float(
                            f.iter()
                                .filter_map(|x| x.as_f64().map(|n| n as f32))
                                .collect(),
                        )
                    }),
                    EncFmt::Base64 => cell.as_str().map(|s| VectorData::Base64(s.to_string())),
                    // int8/uint8/binary/ubinary arrive as JSON integer arrays.
                    _ => cell.as_array().map(|f| {
                        VectorData::Int(
                            f.iter()
                                .filter_map(|x| x.as_i64().map(|n| n as i32))
                                .collect(),
                        )
                    }),
                };
                if let Some(vd) = vd {
                    item.vectors.insert(*enc, vd);
                }
            }
            item
        })
        .collect();
    // Billed count: absent or `null` is no usage; an unreadable one is refused alone
    // (`refused_count`) and bills nothing, while the vectors are served.
    let usage = crate::codec::usage_count::billed_count_opt(
        v.get(keys::META).and_then(|m| m.get(keys::BILLED_UNITS)),
        keys::INPUT_TOKENS,
    )
    .unwrap_or_else(|unreadable| {
        crate::codec::usage_count::refused_count(super::VENDOR_NAME, &unreadable);
        None
    })
    .map(|n| busbar_contract::billing::TokenUsage {
        input: n,
        ..Default::default()
    });
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
    let documents = crate::codec::rerank_wire::read_documents(wire.get(keys::DOCUMENTS));
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
        results: crate::codec::rerank_wire::read_results(v.get(keys::RESULTS)),
        // The priced quantity (`RerankResp::billing`). Absent or `null` is `None` (the flat marker);
        // an unreadable count is refused alone (`refused_count`) and the ranking is served on the flat marker.
        search_units: crate::codec::usage_count::billed_count_opt(
            v.get(keys::META).and_then(|m| m.get(keys::BILLED_UNITS)),
            keys::SEARCH_UNITS,
        )
        .unwrap_or_else(|unreadable| {
            crate::codec::usage_count::refused_count(super::VENDOR_NAME, &unreadable);
            None
        }),
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
        ..crate::codec::leaf_codec::LeafCodecs::NONE
    };
