//! Converse `Citation` projection: the neutral [`crate::codec::ir::IrCitation`] to and from the Bedrock

/// Project an [`crate::codec::ir::IrCitation`] into the Converse `Citation` object, the same field
use crate::codec::keys;
pub(super) fn write_bedrock_citation(
    c: &crate::codec::ir::IrCitation,
) -> Option<serde_json::Value> {
    let mut obj = serde_json::Map::new();

    let title = c.title.as_deref().filter(|s| !s.is_empty());
    let url = c.url.as_deref().filter(|s| !s.is_empty());
    if let Some(t) = title {
        obj.insert(keys::TITLE.to_string(), serde_json::json!(t));
    }

    if let Some(quoted) = c.cited_text.as_deref().filter(|s| !s.is_empty()) {
        obj.insert(
            super::SOURCE_CONTENT.to_string(),
            serde_json::json!([{ (keys::TEXT): quoted }]),
        );
    }

    let answer_text_offsets = c.kind.as_deref() == Some("web_search_result_location")
        || (c.kind.is_none() && url.is_some());
    let char_offsets = !answer_text_offsets
        && !matches!(
            c.kind.as_deref(),
            Some(keys::PAGE_LOCATION)
                | Some(keys::CONTENT_BLOCK_LOCATION)
                | Some(keys::SEARCH_RESULT_LOCATION)
        );
    if answer_text_offsets && (c.start_index.is_some() || c.end_index.is_some()) {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::TEXT,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [dropped = 1,],
            "dropping answer-text citation offsets on a bedrock egress: the web location carries \
             the source URL but has no answer-text span"
        );
    }
    let mut located = false;
    if char_offsets {
        if let (Some(start), Some(end)) = (c.start_index, c.end_index) {
            if start >= 0 && end >= start {
                obj.insert(
                    super::LOCATION.to_string(),
                    serde_json::json!({
                        (super::DOCUMENT_CHAR): {
                            (super::DOCUMENT_INDEX): c.document_index.unwrap_or(0).max(0),
                            (keys::START): start,
                            (keys::END): end,
                        }
                    }),
                );
                located = true;
            }
        }
    }

    if !located && c.kind.as_deref() == Some(keys::SEARCH_RESULT_LOCATION) {
        if let (Some(idx), Some(start), Some(end)) = (c.document_index, c.start_index, c.end_index)
        {
            if idx >= 0 && start >= 0 && end >= start {
                obj.insert(
                    super::LOCATION.to_string(),
                    serde_json::json!({
                        (super::SEARCH_RESULT_LOCATION_CAMEL): {
                            (super::SEARCH_RESULT_INDEX): idx,
                            (keys::START): start,
                            (keys::END): end,
                        }
                    }),
                );
                located = true;
            }
        }
    }

    if c.kind.as_deref() == Some(keys::SEARCH_RESULT_LOCATION) {
        if let Some(source) = c.url.as_deref() {
            obj.insert(keys::SOURCE.to_string(), serde_json::json!(source));
        }
    } else if let Some(u) = url {
        if located {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::TEXT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [url = %u, ],
                "dropping citation `url` on a bedrock egress: the Converse `CitationLocation` is a \
                 union and this citation's `documentChar` member already fills it");
        } else {
            let mut web = serde_json::Map::new();
            web.insert(keys::URL.to_string(), serde_json::json!(u));
            if let Some(d) = c.domain.as_deref().filter(|d| !d.is_empty()) {
                web.insert(keys::DOMAIN.to_string(), serde_json::json!(d));
            }
            obj.insert(
                super::LOCATION.to_string(),
                serde_json::json!({ (keys::WEB): serde_json::Value::Object(web) }),
            );
        }
    }

    (!obj.is_empty()).then_some(serde_json::Value::Object(obj))
}

/// Read one Converse `Citation` (buffered `citationsContent.citations[]` entry, or streamed
pub(super) fn read_bedrock_citation(c: &serde_json::Value) -> crate::codec::ir::IrCitation {
    let loc = c.get(super::LOCATION);
    let member = |k: &str| loc.and_then(|l| l.get(k)).filter(|v| v.is_object());
    let int =
        |v: Option<&serde_json::Value>, k: &str| v.and_then(|o| o.get(k)).and_then(|n| n.as_i64());

    const OFFSET_LOCATIONS: [(&str, &str, &str); 4] = [
        (super::DOCUMENT_CHAR, "char_location", super::DOCUMENT_INDEX),
        ("documentPage", keys::PAGE_LOCATION, super::DOCUMENT_INDEX),
        (
            "documentChunk",
            keys::CONTENT_BLOCK_LOCATION,
            super::DOCUMENT_INDEX,
        ),
        (
            super::SEARCH_RESULT_LOCATION_CAMEL,
            keys::SEARCH_RESULT_LOCATION,
            super::SEARCH_RESULT_INDEX,
        ),
    ];
    let offsets = OFFSET_LOCATIONS.iter().find_map(|(wire, kind, index)| {
        member(wire).map(|m| {
            (
                Some(*kind),
                int(Some(m), index),
                int(Some(m), keys::START),
                int(Some(m), keys::END),
                if *kind == keys::SEARCH_RESULT_LOCATION {
                    c.get(keys::SOURCE)
                        .and_then(|source| source.as_str())
                        .map(String::from)
                } else {
                    None
                },
            )
        })
    });
    let (kind, document_index, start_index, end_index, url) =
        offsets.unwrap_or_else(|| match member(keys::WEB) {
            Some(m) => (
                Some("web_search_result_location"),
                None,
                None,
                None,
                m.get(keys::URL).and_then(|u| u.as_str()).map(String::from),
            ),
            None => (None, None, None, None, None),
        });
    let quoted: Vec<&str> = c
        .get(super::SOURCE_CONTENT)
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get(keys::TEXT).and_then(|t| t.as_str()))
                .collect()
        })
        .unwrap_or_default();

    let domain = member(keys::WEB)
        .and_then(|m| m.get(keys::DOMAIN))
        .and_then(|d| d.as_str())
        .map(String::from);
    crate::codec::ir::IrCitation {
        domain,
        kind: kind.map(String::from),
        cited_text: (!quoted.is_empty()).then(|| quoted.concat()),
        title: c
            .get(keys::TITLE)
            .and_then(|t| t.as_str())
            .map(String::from),
        url,
        document_index,
        start_index,
        end_index,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    }
}

/// Read a `citationsContent` block (`{content: [{text}], citations: [Citation]}`) into one IR
pub(super) fn read_bedrock_citations_content(v: &serde_json::Value) -> crate::codec::ir::IrBlock {
    let text: String = v
        .get(keys::CONTENT)
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get(keys::TEXT).and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default();
    let citations = v
        .get(keys::CITATIONS)
        .and_then(|c| c.as_array())
        .map(|a| a.iter().map(read_bedrock_citation).collect())
        .unwrap_or_default();
    crate::codec::ir::IrBlock::Text {
        text,
        cache_control: None,
        citations,
        refusal: false,
    }
}
