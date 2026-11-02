// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Embeddings IR, cross-protocol across OpenAI, Cohere, Gemini, Bedrock (not Anthropic: no embeddings

use busbar_contract::billing::{Billing, TokenUsage};
use busbar_contract::ir::SourceScopedExtra;
use std::collections::BTreeMap;

/// Output vector encoding. Also the KEY into [`EmbeddingItem::vectors`], so multi-encoding responses
/// are lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EncFmt {
    Float,
    Base64,
    Int8,
    Uint8,
    Binary,
    Ubinary,
}

/// One vector's data in a given encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum VectorData {
    Float(Vec<f32>),
    Int(Vec<i32>),
    Base64(String),
}

/// The input to embed. Text / token-arrays / images cover OpenAI/Cohere/Gemini/Bedrock; anything
/// exotic (Cohere v2 `inputs` mixed content) rides `extra` for lossless round-trip.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbInput {
    Text(Vec<String>),
    /// Cohere/Gemini/Bedrock accept token-array input; no 1.5.0 ingress reader constructs this
    /// variant yet, but the superset IR must be able to express it once one does.
    #[allow(dead_code)]
    Tokens(Vec<Vec<u32>>),
    /// Cohere/Gemini/Bedrock accept image input for embedding; no 1.5.0 ingress reader constructs
    /// this variant yet, but the superset IR must be able to express it once one does.
    #[allow(dead_code)]
    Images(Vec<String>),
}

impl Default for EmbInput {
    fn default() -> Self {
        EmbInput::Text(Vec::new())
    }
}

/// Embeddings request IR — the superset over all four providers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EmbeddingsReq {
    pub model: String,
    pub input: EmbInput,
    pub input_type: Option<String>,
    pub task_type: Option<String>,
    pub title: Option<String>,
    pub dimensions: Option<u32>,
    pub encoding_formats: Vec<EncFmt>,
    pub truncate: Option<String>,
    pub max_tokens: Option<u32>,
    pub normalize: Option<bool>,
    pub user: Option<String>,
    pub priority: Option<i32>,
    pub extra: SourceScopedExtra,
}

/// The embeddings family's walk, this IR's answer to [`busbar_contract::ir::facts::IrFacts`], the

impl busbar_contract::ir::facts::IrFacts for EmbeddingsReq {
    fn verb(&self) -> busbar_contract::operation::OpVerb {
        busbar_contract::operation::OpVerb::EMBEDDINGS
    }

    fn wants_stream(&self) -> bool {
        false
    }

    fn end_user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    fn shape(&self) -> busbar_contract::ir::facts::Shape {
        super::one_turn_shape(self)
    }

    fn content(&self) -> Vec<busbar_contract::ir::facts::ContentItem<'_>> {
        use busbar_contract::ir::facts::{ContentItem, Slot, OPAQUE_CONTENT_MARKER};
        use std::borrow::Cow;
        let mut out = Vec::new();
        match &self.input {
            EmbInput::Text(strings) => {
                for s in strings {
                    out.push(ContentItem::Text {
                        author: "user",
                        slot: Slot::Turn(0),
                        text: Cow::Borrowed(s.as_str()),
                    });
                }
            }

            EmbInput::Tokens(arrays) => {
                for _ in arrays {
                    out.push(ContentItem::Opaque {
                        author: "user",
                        slot: Slot::Turn(0),
                        label: "tokens",
                        marker: OPAQUE_CONTENT_MARKER,
                    });
                }
            }

            EmbInput::Images(images) => {
                for _ in images {
                    out.push(ContentItem::Opaque {
                        author: "user",
                        slot: Slot::Turn(0),
                        label: "image",
                        marker: OPAQUE_CONTENT_MARKER,
                    });
                }
            }
        }

        if let Some(title) = &self.title {
            out.push(ContentItem::Text {
                author: "user",
                slot: Slot::Turn(0),
                text: Cow::Borrowed(title.as_str()),
            });
        }
        out
    }
}

/// One embedding, positionally aligned to the request input at `index`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EmbeddingItem {
    pub index: usize,
    /// Keyed by encoding — the losslessness crux (multi-encoding responses keep every vector).
    pub vectors: BTreeMap<EncFmt, VectorData>,
    pub shape: Option<Vec<u32>>,
}

/// Embeddings response IR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EmbeddingsResp {
    pub id: Option<String>,
    pub model: Option<String>,
    pub object_kind: Option<String>,
    pub embeddings: Vec<EmbeddingItem>,
    pub input_echo: Option<Vec<String>>,
    pub usage: Option<TokenUsage>,
    pub extra: SourceScopedExtra,
}

impl EmbeddingsResp {
    /// Billing projection: embeddings are token-metered (input tokens; Bedrock returns none → `None`).
    pub fn billing(&self) -> Option<Billing> {
        self.usage.clone().map(Billing::Tokens)
    }
}

#[cfg(test)]
#[path = "tests/embeddings_tests.rs"]
mod tests;
