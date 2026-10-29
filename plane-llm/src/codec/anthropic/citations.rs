// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Anthropic citations: the location-type union read into and written from the neutral IrCitation.

use super::*;
use crate::codec::keys;

/// Map one raw citation object to a neutral [`crate::codec::ir::IrCitation`], keeping the source
/// verbatim in `raw` so the Anthropic writer re-emits it byte-exact while a cross-protocol writer
/// has the neutral coordinates. The `type` variants name start/end differently (char/page/block
/// index, web-search `encrypted_index`); each is read into the shared neutral slots by `type`.
pub(super) fn read_citation(val: &serde_json::Value) -> crate::codec::ir::IrCitation {
    let kind = val
        .get(keys::TYPE)
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cited_text = val
        .get(super::CITED_TEXT)
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // `document_title` (document-location variants) OR `title` (web_search_result_location).
    let title = val
        .get(super::DOCUMENT_TITLE)
        .or_else(|| val.get(keys::TITLE))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // `url` (web_search_result_location) OR `source` (search_result_location, the caller-supplied
    // URI); reading only `url` loses a search-result citation's provenance on a foreign egress.
    let url = val
        .get(keys::URL)
        .or_else(|| val.get(keys::SOURCE))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // `document_index` (document-location variants) OR `search_result_index`.
    let document_index = val
        .get(super::DOCUMENT_INDEX)
        .or_else(|| val.get(super::SEARCH_RESULT_INDEX))
        .and_then(|v| v.as_i64());
    // Per-variant start/end field names collapse into the shared neutral slots.
    let start_index = val
        .get(super::START_CHAR_INDEX)
        .or_else(|| val.get(super::START_PAGE_NUMBER))
        .or_else(|| val.get(super::START_BLOCK_INDEX))
        .and_then(|v| v.as_i64());
    let end_index = val
        .get(super::END_CHAR_INDEX)
        .or_else(|| val.get(super::END_PAGE_NUMBER))
        .or_else(|| val.get(super::END_BLOCK_INDEX))
        .and_then(|v| v.as_i64());
    let encrypted_index = val
        .get(super::ENCRYPTED_INDEX)
        .and_then(|v| v.as_str())
        .map(str::to_string);
    crate::codec::ir::IrCitation {
        domain: None,
        kind,
        cited_text,
        title,
        url,
        document_index,
        start_index,
        end_index,
        encrypted_index,
        // VERBATIM source object → byte-exact same-protocol re-emission.
        raw: Some(val.clone()),
        ..Default::default()
    }
}

/// True when `raw` is an Anthropic citation object (its `type` is a known variant). Gates the
/// byte-exact passthrough so a foreign `raw` (Gemini `citationSources[]`) is rebuilt from neutral
/// fields.
pub(super) fn is_anthropic_citation_shape(raw: &serde_json::Value) -> bool {
    matches!(
        raw.get(keys::TYPE).and_then(|v| v.as_str()),
        Some(
            CITATION_TYPE_CHAR
                | CITATION_TYPE_PAGE
                | CITATION_TYPE_CONTENT_BLOCK
                | CITATION_TYPE_WEB_SEARCH
                | CITATION_TYPE_SEARCH_RESULT
        )
    )
}

/// Map a neutral [`crate::codec::ir::IrCitation`] to an Anthropic citation object. A present `raw`
/// is emitted verbatim, so Anthropic to IR to Anthropic is byte-exact; only when it is absent
/// (a citation synthesized on a cross-protocol hop) is the object built from the neutral fields,
/// with `kind` choosing the variant and its field names.
pub(super) fn write_citation(c: &crate::codec::ir::IrCitation) -> serde_json::Value {
    // Re-emit `raw` verbatim only when it is an Anthropic citation (an Anthropic `type` tag); a
    // foreign `raw` (e.g. Gemini `uri`/`startIndex`) falls through to the built shape.
    if let Some(raw) = &c.raw {
        if is_anthropic_citation_shape(raw) {
            return raw.clone();
        }
    }
    let mut obj = serde_json::Map::new();
    let kind = c.kind.as_deref().unwrap_or(CITATION_TYPE_WEB_SEARCH);
    obj.insert(keys::TYPE.to_string(), serde_json::json!(kind));
    if let Some(t) = &c.cited_text {
        obj.insert(super::CITED_TEXT.to_string(), serde_json::json!(t));
    }
    match kind {
        CITATION_TYPE_PAGE => document_location(
            c,
            &mut obj,
            super::START_PAGE_NUMBER,
            super::END_PAGE_NUMBER,
        ),
        CITATION_TYPE_CONTENT_BLOCK => document_location(
            c,
            &mut obj,
            super::START_BLOCK_INDEX,
            super::END_BLOCK_INDEX,
        ),
        // A search-result citation names its passage by `source` + `search_result_index` and its
        // span by content-block indices; char-location names would not read as one.
        CITATION_TYPE_SEARCH_RESULT => {
            if let Some(u) = &c.url {
                obj.insert(keys::SOURCE.to_string(), serde_json::json!(u));
            }
            if let Some(t) = &c.title {
                obj.insert(keys::TITLE.to_string(), serde_json::json!(t));
            }
            if let Some(di) = c.document_index {
                obj.insert(
                    super::SEARCH_RESULT_INDEX.to_string(),
                    serde_json::json!(di),
                );
            }
            if let Some(s) = c.start_index {
                obj.insert(super::START_BLOCK_INDEX.to_string(), serde_json::json!(s));
            }
            if let Some(e) = c.end_index {
                obj.insert(super::END_BLOCK_INDEX.to_string(), serde_json::json!(e));
            }
        }
        CITATION_TYPE_WEB_SEARCH => {
            if let Some(u) = &c.url {
                obj.insert(keys::URL.to_string(), serde_json::json!(u));
            }
            if let Some(t) = &c.title {
                obj.insert(keys::TITLE.to_string(), serde_json::json!(t));
            }
            if let Some(ei) = &c.encrypted_index {
                obj.insert(super::ENCRYPTED_INDEX.to_string(), serde_json::json!(ei));
            }
        }
        // "char_location" and any unknown/None kind default to the char-location field names.
        _ => document_location(c, &mut obj, super::START_CHAR_INDEX, super::END_CHAR_INDEX),
    }
    serde_json::Value::Object(obj)
}

/// The members every Anthropic document-location variant shares — `document_index`,
/// `document_title` — then its span under the variant's own start/end names (char, page or block).
fn document_location(
    c: &crate::codec::ir::IrCitation,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    start_key: &str,
    end_key: &str,
) {
    if let Some(di) = c.document_index {
        obj.insert(super::DOCUMENT_INDEX.to_string(), serde_json::json!(di));
    }
    if let Some(t) = &c.title {
        obj.insert(super::DOCUMENT_TITLE.to_string(), serde_json::json!(t));
    }
    if let Some(s) = c.start_index {
        obj.insert(start_key.to_string(), serde_json::json!(s));
    }
    if let Some(e) = c.end_index {
        obj.insert(end_key.to_string(), serde_json::json!(e));
    }
}
