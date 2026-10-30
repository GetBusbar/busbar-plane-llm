// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `url_citation` `annotations` wire to IR-citation mapping shared by the Chat and Responses
//! codecs, in a module named for the mechanism so neither dialect imports the other (design F3
//! self-contained).
//!
//! The wires differ in shape, each with its own builder: Responses flattens the citation onto the
//! entry (`UrlCitationBody`), Chat nests it under a `url_citation` object
//! (`ChatCompletionResponseMessage.annotations`). The citation rules (which sources qualify, how a
//! span is resolved) are shared.

/// Build a Responses `annotations` array from the IR citations annotating a span of assistant text
/// (the flat `UrlCitationBody`). See [`chat_url_annotations`] for Chat.
///
/// `text` is the one block the citations annotate and `base` is where it starts in the message's
/// full content string (Chat joins every text block, Responses keeps one part per block and passes
/// `0`). Carried and quote-recovered offsets are block-relative, so `base` applies to both.
///
/// `url_citation` requires `url`, `title`, `start_index` and `end_index`, which IR sources do not all
/// carry (an Anthropic `web_search_result_location` has url and title but no offsets, a Gemini
/// `citationSources[]` entry has offsets but no title). The rule is to never invent a fact:
///
/// - `url` is required. A citation without one is a document reference, a different wire shape
///   (`file_citation`, keyed by a `file_id` the IR lacks), so it is omitted rather than mis-shaped.
/// - Offsets come from the citation, else are recovered by locating the quoted `cited_text` in the
///   assembled text; a quote that is absent or appears more than once is ambiguous and omitted.
/// - `title` falls back to the url: the same datum re-presented, and what a client renders anyway.
///
/// `start_index: 0, end_index: 0` or a placeholder title would trade a silent drop for silent
/// fabrication, worse for a fidelity layer.
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
/// `ChatCompletionResponseMessage.annotations` items nest the citation under a `url_citation` object
/// rather than flattening it. The two forms must not be interchanged, and [`read_url_annotations`]
/// recognizes the nested one, which makes a Chat write-then-read hop lossless.
///
/// Used by both chat arms. The streaming arm has no assembled text to resolve a span against, so it
/// passes an empty `text` and `base` 0; an unresolvable span is omitted rather than fabricated.
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
/// offset rules cannot drift. `None` when the source carries no usable span (each caller decides).
fn citation_span(text: &str, base: usize, c: &crate::codec::ir::IrCitation) -> Option<(i64, i64)> {
    // Carried offsets span the response text only for a url/web-search citation (OpenAI
    // `url_citation`, Gemini `citationSources[]`, both read as `web_search_result_location`) or an
    // untyped one. Anthropic's document kinds carry offsets into the cited document (`char_location`
    // chars, `page_location` pages, block indices), which as an answer-text span would point at the
    // wrong characters.
    let text_span_kind = matches!(c.kind.as_deref(), None | Some("web_search_result_location"));
    let carried = if text_span_kind {
        (c.start_index, c.end_index)
    } else {
        (None, None)
    };
    match carried {
        // `saturating_add`, not `+`: `s`/`e` are upstream-controlled `i64`, so `i64::MAX + base`
        // would panic in debug or wrap in release. Same cure as `billable_tokens`.
        (Some(s), Some(e)) if s >= 0 && e >= s => {
            Some((s.saturating_add(base as i64), e.saturating_add(base as i64)))
        }
        // Recover the span from the quote when no offsets were carried, only if it occurs exactly
        // once (two matches make the anchor ambiguous).
        _ => c
            .cited_text
            .as_deref()
            .filter(|q| !q.is_empty())
            .and_then(|q| {
                // `str::find` and `q.len()` are byte offsets; the IR contract is characters (see
                // `IrCitation::start_index`). `find` returns a char boundary so the slice is valid;
                // only the emitted span needs converting.
                let first = text.find(q)?;
                if text[first + q.len()..].contains(q) {
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
///
/// Both published shapes are accepted: Chat nests the fields under a `url_citation` object, while
/// Responses' `UrlCitationBody` flattens `url`/`title`/`start_index`/`end_index` onto the entry.
/// Recognizing only the nested one dropped every Responses citation, even Responses to Responses,
/// where this reader consumes the flat bytes `url_annotations` just wrote. The nested object wins
/// when present, so a Chat entry is never re-read as flat.
///
/// Offsets are carried. `IrCitation::start_index`/`end_index` are character offsets by contract and
/// OpenAI documents its own the same way ("the index of the first character of the URL citation in
/// the message", in both Chat and Responses), so both copy across unconverted. When they were
/// dropped, every Chat-backend citation vanished for a Responses client (the writer requires a
/// span) and Responses to Chat lost its offsets. A negative or inverted pair is not a span and stays
/// `None` rather than repaired.
pub fn read_url_annotations(annotations: &serde_json::Value) -> Vec<crate::codec::ir::IrCitation> {
    let mut out = Vec::new();
    let Some(arr) = annotations.as_array() else {
        return out;
    };
    for entry in arr {
        if entry.get("type").and_then(|t| t.as_str()) != Some("url_citation") {
            continue;
        }
        // Chat nests under `url_citation`; Responses (`UrlCitationBody`) flattens onto the entry.
        let citation = entry.get("url_citation").unwrap_or(entry);
        // Never invent a fact: an entry with no usable url is skipped, as `url_annotations` does on write.
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
/// Responses lane (or back) though the id is valid on both. Both readers store `{"file_id": <id>}`;
/// this accepts either tag, so a writer calls it instead of comparing its own. Any other vendor (a
/// Bedrock `s3Location`, an Anthropic Files id) is `None`: those do not resolve at OpenAI.
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
