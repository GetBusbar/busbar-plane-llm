// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE RERANK WIRE SHAPE TWO DIALECTS SPEAK ALIKE: the `documents[]` a rerank request ranks and
//! the `results[]` its answer carries.

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
/// ranking must be refused, without estimating a missing or unreadable count.
pub fn read_search_units(value: &Value, protocol: &'static str) -> Option<u64> {
    match crate::codec::usage_count::exact_count_opt(
        value
            .get(keys::META)
            .and_then(|meta| meta.get(keys::BILLED_UNITS)),
        keys::SEARCH_UNITS,
    ) {
        Ok(Some(n)) if n.micros() % 1_000_000 == 0 => u64::try_from(n.micros() / 1_000_000).ok(),
        Ok(_) => None,
        Err(unreadable) => {
            crate::codec::usage_count::refused_count(protocol, &unreadable);
            None
        }
    }
}

/// The token-usage tap's existing open-unit carrier, using the same class as `RerankResp::billing`.
pub fn extract_usage(
    body: &[u8],
    protocol: &'static str,
) -> Result<Option<busbar_contract::billing::TokenUsage>, crate::codec::cells::UsageTapFault> {
    let value: Value =
        crate::codec::json::parse(body).map_err(|_| crate::codec::cells::UsageTapFault::Decode)?;
    Ok(crate::codec::usage_count::token_usage(
        protocol,
        value
            .get(keys::META)
            .and_then(|meta| meta.get(keys::BILLED_UNITS)),
        &[(
            keys::SEARCH_UNITS,
            crate::codec::ir::rerank::SEARCH_UNITS_CLASS,
        )],
    ))
}

/// Exact carrier for a fractional rerank count or an explicit count refusal.
pub fn exact_usage(
    value: &Value,
    protocol: &'static str,
) -> Option<busbar_contract::billing::TokenUsage> {
    crate::codec::usage_count::token_usage(
        protocol,
        value
            .get(keys::META)
            .and_then(|m| m.get(keys::BILLED_UNITS)),
        &[(
            keys::SEARCH_UNITS,
            crate::codec::ir::rerank::SEARCH_UNITS_CLASS,
        )],
    )
    .filter(|u| {
        !u.refused_counts.is_empty()
            || u.exact_counts
                .as_ref()
                .is_some_and(|e| e.values().any(|n| n.micros() % 1_000_000 != 0))
    })
}
