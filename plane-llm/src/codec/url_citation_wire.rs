// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `url_citation` `annotations` wire to IR-citation mapping shared by the Chat and Responses
//! codecs, in a module named for the mechanism so neither dialect imports the other (design F3
//! self-contained).

/// Build a Responses `annotations` array from the IR citations annotating a span of assistant text
/// (the flat `UrlCitationBody`).
pub fn url_annotations(
    text: &str,
    base: usize,
    citations: &[crate::codec::ir::IrCitation],
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for c in citations {
        let Some(url) = c.url.as_deref().filter(|u| !u.is_empty()) else {
            continue;
        };
        let Some((start, end)) = citation_span(text, base, c) else {
            continue;
        };
        out.push(serde_json::json!({
            "type": "url_citation",
            "url": url,
            "title": citation_title(c, url),
            "start_index": start,
            "end_index": end,
        }));
    }
    out
}

/// Build a Chat `annotations` array. Same citation rules as [`url_annotations`], different shape:
pub fn chat_url_annotations(
    text: &str,
    base: usize,
    citations: &[crate::codec::ir::IrCitation],
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for c in citations {
        let Some(url) = c.url.as_deref().filter(|u| !u.is_empty()) else {
            continue;
        };
        let mut uc = serde_json::Map::new();
        uc.insert("url".to_string(), serde_json::json!(url));
        uc.insert(
            "title".to_string(),
            serde_json::json!(citation_title(c, url)),
        );
        if let Some((start, end)) = citation_span(text, base, c) {
            uc.insert("start_index".to_string(), serde_json::json!(start));
            uc.insert("end_index".to_string(), serde_json::json!(end));
        }
        out.push(serde_json::json!({
            "type": "url_citation",
            "url_citation": serde_json::Value::Object(uc),
        }));
    }
    out
}

/// `title` falls back to the url: the same datum re-presented, and what a client renders anyway.
fn citation_title<'a>(c: &'a crate::codec::ir::IrCitation, url: &'a str) -> &'a str {
    c.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(url)
}

/// Resolve a citation's character span within `text`, shifted by `base`; shared by both shapes so the
/// offset rules cannot drift.
pub(crate) fn citation_span(
    text: &str,
    base: usize,
    c: &crate::codec::ir::IrCitation,
) -> Option<(i64, i64)> {
    let text_span_kind = matches!(c.kind.as_deref(), None | Some("web_search_result_location"));
    let carried = if text_span_kind {
        (c.start_index, c.end_index)
    } else {
        (None, None)
    };
    match carried {
        (Some(s), Some(e)) if s >= 0 && e >= s => {
            Some((s.saturating_add(base as i64), e.saturating_add(base as i64)))
        }

        _ => c
            .cited_text
            .as_deref()
            .filter(|q| !q.is_empty())
            .and_then(|q| {
                let first = text.find(q)?;
                let next_start = first + text[first..].chars().next()?.len_utf8();
                if text[next_start..].contains(q) {
                    return None;
                }
                let start_ch = text[..first].chars().count();
                let len_ch = q.chars().count();
                Some(((base + start_ch) as i64, (base + start_ch + len_ch) as i64))
            }),
    }
}

/// Read an OpenAI-family `annotations` array (`url_citation` entries) into IR citations, shared by
/// the Chat and Responses readers (mirroring `url_annotations` in the write direction).
pub fn read_url_annotations(annotations: &serde_json::Value) -> Vec<crate::codec::ir::IrCitation> {
    let mut out = Vec::new();
    let Some(arr) = annotations.as_array() else {
        return out;
    };
    for entry in arr {
        if entry.get("type").and_then(|t| t.as_str()) != Some("url_citation") {
            continue;
        }

        let citation = entry.get("url_citation").unwrap_or(entry);

        let Some(url) = citation
            .get("url")
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        let title = citation
            .get("title")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(String::from);
        let (start_index, end_index) = match (
            citation.get("start_index").and_then(|v| v.as_i64()),
            citation.get("end_index").and_then(|v| v.as_i64()),
        ) {
            (Some(s), Some(e)) if s >= 0 && e >= s => (Some(s), Some(e)),
            _ => (None, None),
        };
        out.push(crate::codec::ir::IrCitation {
            domain: None,
            kind: Some("web_search_result_location".to_string()),
            cited_text: None,
            title,
            url: Some(url.to_string()),
            document_index: None,
            start_index,
            end_index,
            encrypted_index: None,
            raw: None,
            ..Default::default()
        });
    }
    out
}

/// The OpenAI Files API id an IR attachment references, if any (SHR-03). OpenAI Chat
/// (`file.file_id`) and Responses (`input_file.file_id`, `input_image.file_id`) name one namespace,
/// the organisation's Files uploads, but each reader tags the opaque
/// [`crate::codec::ir::IrImageSource::Vendor`] with its own dialect name (`"openai"`, `"responses"`)
/// and each writer re-emitted only its own tag, so a Chat caller's uploaded PDF never reached a
/// Responses lane (or back) though the id is valid on both.
pub fn files_api_id(source: &crate::codec::ir::IrImageSource) -> Option<&str> {
    match source {
        crate::codec::ir::IrImageSource::Vendor { vendor, value }
            if FILES_API_VENDOR_TAGS.contains(vendor) =>
        {
            value
                .get("file_id")
                .and_then(|i| i.as_str())
                .filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

/// The `Vendor` tags naming the OpenAI Files namespace (the Chat and Responses readers'), kept in
/// step with their `VENDOR_NAME` consts by `openai_file_vendor_tags_are_the_dialect_names`.
pub const FILES_API_VENDOR_TAGS: [&str; 2] = ["openai", "responses"];
