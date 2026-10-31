// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Rerank IR (the seventh operation), cross-protocol across Cohere (`/v2/rerank`) and Bedrock (rerank
//! models via `InvokeModel`), the only protocols with a rerank surface. The wires are near-identical
//! (query + documents in, index + relevance_score out), so the IR is a thin normalization; the others
//! 404 via the standard no-handler rule. Search-unit metered: `Billing::Counted` (Cohere bills per
//! search unit, ledgered as the open class `search_units` and priced by the card, item 134).

use busbar_contract::billing::Billing;
use busbar_contract::ir::SourceScopedExtra;

/// Rerank request IR — the superset over both providers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RerankReq {
    pub model: String,
    pub query: String,
    pub documents: Vec<String>,
    pub top_n: Option<u32>,
    pub max_tokens_per_doc: Option<u32>, // Cohere
    /// Cohere/Bedrock `return_documents`: echo each ranked document's text back in the response.
    pub return_documents: Option<bool>,
    pub extra: SourceScopedExtra,
}

/// The rerank family's walk, this IR's answer to [`busbar_contract::ir::facts::IrFacts`]. The `query`
/// and every `document` are caller free text sent upstream verbatim, so both are
/// [`busbar_contract::ir::facts::ContentItem::Text`] for a screening gate. `top_n` and
/// `max_tokens_per_doc` are numeric knobs.
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
/// Anthropic turn's server-side web searches via `IrUsage::to_token_usage`): the key an operator
/// writes under `rate_card.<model>.units` (Cohere's own field name).
pub const SEARCH_UNITS_CLASS: &str = "search_units";

/// Rerank response IR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RerankResp {
    pub id: Option<String>,
    pub results: Vec<RerankResult>,
    pub search_units: Option<u64>, // Cohere meta.billed_units.search_units
    pub extra: SourceScopedExtra,
}

impl RerankResp {
    /// Billing projection: no token meter on either wire. The search units Cohere billed
    /// (`meta.billed_units.search_units`, read exactly by the response reader) are the price's quantity
    /// and reach it as the open class [`SEARCH_UNITS_CLASS`] (item 134), so a 5,000-document rerank no
    /// longer bills like a 1-document one. A response reporting none (Bedrock, or Cohere without
    /// `billed_units`) stays the flat marker.
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
