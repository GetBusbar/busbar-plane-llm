// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE RERANK WIRE SHAPE TWO DIALECTS SPEAK ALIKE: the `documents[]` a rerank request ranks and

use crate::codec::ir::rerank::RerankResult;
use crate::codec::keys;
use serde_json::Value;

/// A ranked text as the wire states it: a bare string, or a `{text}` object.
fn text_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).or_else(|| {
        v.get(keys::TEXT)
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// `documents[]` -> texts in the original order. A malformed entry refuses the request:

pub fn read_documents(v: Option<&Value>) -> Result<Vec<String>, &'static str> {
    let entries = v
        .and_then(Value::as_array)
        .ok_or("rerank documents must be an array")?;
    entries
        .iter()
        .map(|entry| text_of(entry).ok_or("rerank document must be text or a text object"))
        .collect()
}

/// Read every ranked result, refusing malformed entries instead of silently shortening the ranking.
pub fn read_results(v: Option<&Value>) -> Result<Vec<RerankResult>, &'static str> {
    let entries = v
        .and_then(Value::as_array)
        .ok_or("rerank results must be an array")?;
    entries
        .iter()
        .map(|entry| {
            let index = entry
                .get(keys::INDEX)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("rerank result requires an integer index")?;
            let relevance_score = entry
                .get(keys::RELEVANCE_SCORE)
                .and_then(Value::as_f64)
                .ok_or("rerank result requires a numeric relevance_score")?;
            let document = match entry.get(keys::DOCUMENT) {
                None | Some(Value::Null) => None,
                Some(document) => Some(
                    text_of(document)
                        .ok_or("rerank result document must be text or a text object")?,
                ),
            };
            Ok(RerankResult {
                index,
                relevance_score,
                document,
            })
        })
        .collect()
}

/// Search units are independent of the ranked payload. Keep them readable even when a malformed

pub fn read_search_units(value: &Value, protocol: &'static str) -> Option<u64> {
    crate::codec::usage_count::billed_count_opt(
        value
            .get(keys::META)
            .and_then(|meta| meta.get(keys::BILLED_UNITS)),
        keys::SEARCH_UNITS,
    )
    .unwrap_or_else(|unreadable| {
        crate::codec::usage_count::refused_count(protocol, &unreadable);
        None
    })
}

/// The token-usage tap's existing open-unit carrier, using the same class as `RerankResp::billing`.
pub fn extract_usage(
    body: &[u8],
    protocol: &'static str,
) -> Result<Option<busbar_contract::billing::TokenUsage>, busbar_contract::codec::UsageTapFault> {
    let value: Value = crate::codec::json::parse(body)
        .map_err(|_| busbar_contract::codec::UsageTapFault::Decode)?;
    Ok(read_search_units(&value, protocol).map(|count| {
        let mut usage = busbar_contract::billing::TokenUsage::default();
        if count != 0 {
            usage
                .open_units
                .insert(crate::codec::ir::rerank::SEARCH_UNITS_CLASS.into(), count);
        }
        usage
    }))
}
