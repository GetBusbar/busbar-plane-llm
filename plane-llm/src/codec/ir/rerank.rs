// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Rerank IR (the seventh operation), cross-protocol across Cohere (`/v2/rerank`) and Bedrock (rerank

use busbar_contract::billing::Billing;
use busbar_contract::ir::SourceScopedExtra;

/// Rerank request IR — the superset over both providers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RerankReq {
    pub model: String,
    pub query: String,
    pub documents: Vec<String>,
    pub top_n: Option<u32>,
    pub max_tokens_per_doc: Option<u32>,
    /// Cohere/Bedrock `return_documents`: echo each ranked document's text back in the response.
    pub return_documents: Option<bool>,
    pub extra: SourceScopedExtra,
}

/// The rerank family's walk, this IR's answer to [`busbar_contract::ir::facts::IrFacts`]. The `query`
impl busbar_contract::ir::facts::IrFacts for RerankReq {
    fn verb(&self) -> busbar_contract::operation::OpVerb {
        busbar_contract::operation::OpVerb::RERANK
    }

    fn wants_stream(&self) -> bool {
        false
    }

    fn end_user(&self) -> Option<&str> {
        None
    }

    fn shape(&self) -> busbar_contract::ir::facts::Shape {
        super::one_turn_shape(self)
    }

    fn content(&self) -> Vec<busbar_contract::ir::facts::ContentItem<'_>> {
        use busbar_contract::ir::facts::{ContentItem, Slot};
        use std::borrow::Cow;
        let mut out = Vec::with_capacity(1 + self.documents.len());
        out.push(ContentItem::Text {
            author: "user",
            slot: Slot::Turn(0),
            text: Cow::Borrowed(self.query.as_str()),
        });
        for doc in &self.documents {
            out.push(ContentItem::Text {
                author: "user",
                slot: Slot::Turn(0),
                text: Cow::Borrowed(doc.as_str()),
            });
        }
        out
    }
}

/// One ranked hit: the index into the REQUEST's `documents` and its relevance.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RerankResult {
    pub index: usize,
    pub relevance_score: f64,
    /// The echoed document text, present only when the request asked for `return_documents`
    /// (Cohere/Bedrock). Kept so the echo survives a rerank hop instead of being dropped.
    pub document: Option<String>,
}

/// The meter class billed searches are ledgered and priced under (a rerank's search units, and an
pub const SEARCH_UNITS_CLASS: &str = "search_units";

/// Rerank response IR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RerankResp {
    pub id: Option<String>,
    pub results: Vec<RerankResult>,
    pub search_units: Option<u64>,
    pub extra: SourceScopedExtra,
}

impl RerankResp {
    /// Billing projection: no token meter on either wire. The search units Cohere billed
    pub fn billing(&self) -> Option<Billing> {
        Some(match self.search_units {
            Some(count) => Billing::Counted {
                class: SEARCH_UNITS_CLASS.to_string(),
                count,
            },
            None => Billing::Flat,
        })
    }
}

#[cfg(test)]
#[path = "tests/rerank_tests.rs"]
mod tests;
