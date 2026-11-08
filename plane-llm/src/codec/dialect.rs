// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM dialects' shared wire helpers (#83a SD-3: dialect machinery): the OpenAI-family error
//! helpers, the SSE `[DONE]` terminator, the SSE frame probe/parse/write shared by the stream
//! translator and readers, the base62 id alphabet, the message-name sentinel, the top-level `usage`
//! stripper of the same-protocol verbatim writer, and the dropped provider-metadata report.

use crate::codec::framework::ERR_TYPE_SERVER_ERROR;
use crate::codec::protocol::{
    sse_line_spans, ERR_TYPE_API_ERROR, ERR_TYPE_AUTHENTICATION, ERR_TYPE_INSUFFICIENT_QUOTA,
    ERR_TYPE_INVALID_REQUEST, ERR_TYPE_NOT_FOUND, ERR_TYPE_PERMISSION, ERR_TYPE_RATE_LIMIT,
};

/// Machine-readable `code` in a bad-key 401 OpenAI-family envelope, used by [`bearer_error_code`] to
/// mirror the native `authentication_error` to `invalid_api_key` pairing SDKs surface as `error.code`.
pub const CODE_INVALID_API_KEY: &str = "invalid_api_key";

#[cfg(test)]
include!("tests/dialect_provider_signal_context_length_helper.rs");

/// Busbar-internal `extra` key parking OpenAI Chat's per-message `messages[].name` (the optional
/// participant name disambiguating several speakers in one role).
pub const MESSAGE_NAMES_SENTINEL: &str = "__busbar_message_names";

/// The wire path [`MESSAGE_NAMES_SENTINEL`] holds, as a translate attempt's drop names it.
pub const MESSAGE_NAMES_PATH: &str = "messages[].name";

/// File extension to media type, rows two or more dialects read alike. Each dialect lists which
/// groups it recognises beside its own rows: documents (OpenAI Chat, Bedrock), images (OpenAI Chat, Gemini).
pub const DOCUMENT_MEDIA_TYPES: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("csv", "text/csv"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
];
pub const IMAGE_MEDIA_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// The media type of extension `ext` (already lower-cased) in the first of `tables` that lists it.
pub fn media_type(tables: &[&[(&'static str, &'static str)]], ext: &str) -> Option<&'static str> {
    tables
        .iter()
        .flat_map(|t| t.iter())
        .find(|(e, _)| *e == ext)
        .map(|(_, m)| *m)
}

/// How a dialect's egress warn names a dropped control: the field it rides under and the dialect's
/// message.
pub enum DropWarn {
    /// `control = <name>`, with a fixed message.
    Control(&'static str),
    /// `parameter = <name>`, with the message naming it where the template says `{slot}`.
    Parameter(&'static str),
}

/// One warn per dropped control, in the dialect's words (its `drop_warn`, used by
/// `codec::carry::warn_drops`).
pub fn warn_dropped<'a>(dropped: impl IntoIterator<Item = &'a str>, warn: &DropWarn) {
    for name in dropped {
        let path = crate::codec::drops::caller_path(name);
        match warn {
            DropWarn::Control(message) => {
                tracing::warn!(control = name, path = %path, "{}", message);
            }
            DropWarn::Parameter(template) => {
                tracing::warn!(
                    parameter = name,
                    path = %path,
                    "{}",
                    template.replace("{slot}", name)
                );
            }
        }
    }
}

/// The refusal a dialect reader returns for bytes it cannot read as its own shape: a client error
/// with the `ir_parse` lane signal, one constructor so every reader refuses alike.
#[must_use]
pub fn ir_parse_error() -> crate::codec::protocol::IrError {
    crate::codec::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ClientError,
        provider_signal: Some(crate::codec::framework::SIGNAL_IR_PARSE.to_string()),
        retry_after: None,
    }
}

/// Precise context-length prose scan shared by `OpenAiReader::extract_error` and
/// `ResponsesReader::extract_error`.
pub fn context_length_prose_scan(text: &str) -> bool {
    text.contains("maximum context length")
        || text.contains("context length exceeded")
        || text.contains("reduce the length")
        || (text.contains("exceeds") && (text.contains("context") || text.contains("token limit")))
}

/// Map an OpenAI-family error `type` to its canonical `code`, shared by the Chat and `/v1/responses`
pub fn bearer_error_code(error_type: &str) -> serde_json::Value {
    match error_type {
        ERR_TYPE_AUTHENTICATION => serde_json::Value::String(CODE_INVALID_API_KEY.to_string()),

        ERR_TYPE_INSUFFICIENT_QUOTA => {
            serde_json::Value::String(ERR_TYPE_INSUFFICIENT_QUOTA.to_string())
        }
        ERR_TYPE_INVALID_REQUEST
        | ERR_TYPE_PERMISSION
        | ERR_TYPE_NOT_FOUND
        | ERR_TYPE_RATE_LIMIT
        | ERR_TYPE_SERVER_ERROR
        | ERR_TYPE_API_ERROR => serde_json::Value::Null,
        other => {
            let _ = other;
            serde_json::Value::Null
        }
    }
}

/// The OpenAI-style SSE terminator sentinel (`data: [DONE]`). The bare token is matched by the
/// streaming core and several readers; the full framed bytes are emitted on egress.
pub const SSE_DONE_SENTINEL: &str = "[DONE]";
/// The full framed `data: [DONE]\n\n` bytes emitted on egress. See [`SSE_DONE_SENTINEL`].
pub const SSE_DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// Fallback `model` the two bearer-envelope dialects (Chat and Responses) stamp on a cross-protocol
/// answer whose egress supplied none: both schemas make `model` required, so an absent one fails the
/// SDKs' decoders and is a proxy tell.
pub const FALLBACK_MODEL: &str = "gpt-4o";

/// DoS cap on open tool-call accumulators per stream (OpenAI's parallel-tool-call limit, 128), shared
/// by the Chat and Responses readers' open-item sets and the tool-id remap's memo so none drifts.
pub const MAX_OPEN_TOOL_CALLS: usize = 128;

/// Mixed-case base62 alphabet (digits, lowercase, uppercase; no `-`/`_`) for synthesizing opaque ids
/// for protocols whose native ids are flat random tokens (Gemini `responseId`, Responses
/// `msg_`/`fc_`/`resp_` suffixes), single-sourced so the generators cannot drift.
pub const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// The rejection-sampling threshold paired with [`BASE62_ALPHABET`].
pub const BASE62_REJECT_THRESHOLD: u8 = 248;

/// Fill `out` (at most 64 characters) with uniform characters of `alphabet` from the host's entropy
/// by rejection sampling: a bare `byte % 62` over-represents residues 0..7 by ~25%, a statistical tell
/// against a native id, so a byte `>= BASE62_REJECT_THRESHOLD` is discarded.
pub(crate) fn fill_base62(
    host: &dyn crate::codec::cells::CodecHost,
    out: &mut [u8],
    alphabet: &[u8; 62],
) -> bool {
    debug_assert!(out.len() <= 64, "fill_base62 draws at most 64 chars");
    let len = out.len();
    let mut filled = 0usize;
    'outer: while filled < len {
        let mut batch = [0u8; 64];
        let batch = &mut batch[..len];
        if !host.fill_entropy(batch) {
            return false;
        }
        for &byte in batch.iter() {
            if byte >= BASE62_REJECT_THRESHOLD {
                continue;
            }
            out[filled] = alphabet[(byte % 62) as usize];
            filled += 1;
            if filled == len {
                break 'outer;
            }
        }
    }
    true
}

/// Project each message's `(role, content)` into a `(String, String)` pair when both are plain
/// strings, else `None`.
pub fn rewrite_text_pairs(messages: &[serde_json::Value]) -> Option<Vec<(String, String)>> {
    messages
        .iter()
        .map(|m| {
            let role = m
                .get("role")
                .and_then(serde_json::Value::as_str)?
                .to_string();
            let text = m
                .get("content")
                .and_then(serde_json::Value::as_str)?
                .to_string();
            Some((role, text))
        })
        .collect()
}

/// The `event:` name of one SSE frame, borrowed from the frame: the cheap probe for a consumer that
/// needs only the event type.
#[cfg(test)]
pub fn sse_event_type(frame: &[u8]) -> &str {
    let mut name = "";
    for (start, end) in sse_line_spans(frame) {
        if let Some(rest) = frame[start..end].strip_prefix(b"event:") {
            name = std::str::from_utf8(rest).map(str::trim).unwrap_or("");
        }
    }
    name
}

/// Render an IR ToolUse `input` as a wire tool-call `arguments` string. A `Value::String` is emitted
/// verbatim (the reader stores not-valid-JSON upstream arguments as `Value::String(raw)`, and
/// re-serializing would double-encode it); any other `Value` is serialized via the `crate::codec::json`
/// seam.
pub fn tool_arguments_to_string(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::String(s) => s.clone(),
        other => crate::codec::json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// Parse one SSE frame into `(event_type, data_payload)`, both borrowed: the payload is owned only
/// when the frame has several `data:` lines, joined with `\n` per the SSE spec.
pub fn parse_sse_frame(frame: &[u8]) -> Option<(&str, std::borrow::Cow<'_, str>)> {
    let text = std::str::from_utf8(frame).ok()?;
    let mut event_type = "";
    let mut data: Option<std::borrow::Cow<'_, str>> = None;
    for (start, end) in sse_line_spans(frame) {
        let line = &text[start..end];
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data = Some(match data {
                None => std::borrow::Cow::Borrowed(rest),
                Some(mut joined) => {
                    let joined_mut = joined.to_mut();
                    joined_mut.push('\n');
                    joined_mut.push_str(rest);
                    joined
                }
            });
        }
    }
    Some((event_type, data?))
}

/// Append an IR-derived `(event_type, data)` to `out` as ingress SSE bytes: a non-empty `event_type`
pub fn write_sse_frame(out: &mut Vec<u8>, event_type: &str, data: &serde_json::Value) {
    if !event_type.is_empty() {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event_type.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"data: ");

    out.extend_from_slice(&crate::codec::json::to_vec(data).unwrap_or_default());
    out.extend_from_slice(b"\n\n");
}

/// Byte-level removal of a top-level `"usage"` member from a JSON object string, preserving every
/// other byte.
#[cfg(test)]
pub fn strip_top_level_usage_member(json: &str) -> Option<String> {
    let (start, end) = top_level_usage_cut(json)?;
    Some([&json[..start], &json[end..]].concat())
}

/// The byte range [`strip_top_level_usage_member`] removes (the member and exactly one adjacent
/// comma), so a writer can splice around it without building the stripped string.
pub(crate) fn top_level_usage_cut(json: &str) -> Option<(usize, usize)> {
    let bytes = json.as_bytes();
    let n = bytes.len();

    let mut i = 0usize;
    while i < n && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= n || bytes[i] != b'{' {
        return None;
    }
    let obj_open = i;
    i += 1;

    let mut depth = 0usize;

    let mut usage_range: Option<(usize, usize)> = None;

    let mut expect_key = true;

    let mut closed = false;

    while i < n {
        let b = bytes[i];
        match b {
            b'"' => {
                let key_start = i;
                let str_end = busbar_contract::spans::string_end(bytes, i)?;
                if depth == 0 && expect_key {
                    let is_usage = &bytes[key_start..str_end] == b"\"usage\"";

                    let mut j = str_end;
                    while j < n && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j >= n || bytes[j] != b':' {
                        return None;
                    }
                    j += 1;

                    let value_end = busbar_contract::spans::value_end(bytes, j)?;
                    if is_usage {
                        if usage_range.is_some() {
                            return None;
                        }
                        usage_range = Some((key_start, value_end));
                    }
                    i = value_end;
                    expect_key = false;
                    continue;
                }

                i = str_end;
            }
            b'{' | b'[' => {
                depth += 1;
                expect_key = false;
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    if b == b']' {
                        return None;
                    }
                    closed = true;
                    break;
                }
                depth -= 1;
                i += 1;
            }
            b',' => {
                if depth == 0 {
                    expect_key = true;
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    if !closed {
        return None;
    }
    let (start, end) = usage_range?;

    let mut cut_start = start;
    let mut cut_end = end;

    let mut k = start;
    while k > obj_open + 1 && bytes[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k > obj_open + 1 && bytes[k - 1] == b',' {
        cut_start = k - 1;
    } else {
        let mut m = end;
        while m < n && bytes[m].is_ascii_whitespace() {
            m += 1;
        }
        if m < n && bytes[m] == b',' {
            cut_end = m + 1;
        }
    }
    Some((cut_start, cut_end))
}

/// Drop the response-side provider metadata an egress dialect carries and no ingress dialect can
/// express, on the open translate attempt (`codec::drops`), so it does not vanish silently.
pub fn drop_untranslatable_response_metadata(egress: &str, body: &serde_json::Value) {
    let present: Vec<&str> = crate::codec::decl_of(egress)
        .and_then(|d| d.vendor_response_metadata)
        .map(|report| report(body))
        .unwrap_or_default();
    for path in present {
        crate::codec::drops::note(crate::codec::drops::Dropped::new(
            path,
            &crate::codec::diagnostics::PROTO_DROP_PROVIDER_METADATA,
            "dropping response-side provider metadata on the cross-protocol seam: the field \
             named here is a vendor-scoped artifact (a guardrail assessment is an AWS account \
             resource; a harm-category rating uses Google's own vocabulary) and the caller's \
             protocol has no shape to receive it. If this metadata is compliance evidence, route \
             the request to a same-protocol lane, where the upstream body reaches the client \
             verbatim",
        ));
    }
}

#[cfg(test)]
#[path = "tests/dialect_drift_tests.rs"]
mod drift_tests;

#[cfg(test)]
#[path = "tests/dialect_strip_tests.rs"]
mod strip_tests;

#[cfg(test)]
#[path = "tests/dialect_sse_frame_tests.rs"]
mod sse_frame_tests;
