// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM dialects' shared wire helpers (#83a SD-3: dialect machinery): the OpenAI-family error
//! helpers, the SSE `[DONE]` terminator, the SSE frame probe/parse/write shared by the stream
//! translator and readers, the base62 id alphabet, the message-name sentinel, the top-level `usage`
//! stripper of the same-protocol verbatim writer, and the dropped provider-metadata report. Only this
//! plane's dialects speak them; the contract carries just the vocabulary both sides must spell alike
//! (`busbar_contract::protocol`: the error-`type` bank, SSE frame-boundary scan, line walkers).

use crate::codec::framework::ERR_TYPE_SERVER_ERROR;
use busbar_contract::protocol::{
    sse_line_spans, ERR_TYPE_API_ERROR, ERR_TYPE_AUTHENTICATION, ERR_TYPE_INSUFFICIENT_QUOTA,
    ERR_TYPE_INVALID_REQUEST, ERR_TYPE_NOT_FOUND, ERR_TYPE_PERMISSION, ERR_TYPE_RATE_LIMIT,
};

/// Machine-readable `code` in a bad-key 401 OpenAI-family envelope, used by [`bearer_error_code`] to
/// mirror the native `authentication_error` to `invalid_api_key` pairing SDKs surface as `error.code`.
/// Also matched by the Responses stream classifier (`class_for_response_failed`).
pub const CODE_INVALID_API_KEY: &str = "invalid_api_key";

/// Busbar-internal `provider_signal` label for a context-length result (the lane label, not the
/// OpenAI wire code, and distinct from `proxy::PROVIDER_CODE_CONTEXT_LENGTH`). Its referent is the
/// bearer-envelope classifier test mirror (`dialect_classify::bearer`).
#[cfg(test)]
pub const PROVIDER_SIGNAL_CONTEXT_LENGTH: &str = "context_length";

/// Busbar-internal `extra` key parking OpenAI Chat's per-message `messages[].name` (the optional
/// participant name disambiguating several speakers in one role).
///
/// A sentinel, not an `IrMessage` field: no other protocol (Anthropic, Gemini, Bedrock, Cohere,
/// Responses) models a participant name, so a first-class field would be read or written by one
/// dialect only. `extra` gives the right scope:
///
/// * Same-protocol (including a pool-alias route that re-serializes): the writer re-emits `name` on
///   each message, so it no longer disappears when a route rewrites the model.
/// * Cross-protocol: `extra` is cleared at the seam and the one drop path (`codec::drops`) names
///   [`MESSAGE_NAMES_PATH`], so the loss is warned and audited.
///
/// Value: an object keyed by the message's index in `IrRequest.messages` (decimal string) to the
/// name; keyed by index so one named message costs one entry and a `null` hole cannot misalign lookup.
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
/// message. Data per dialect; [`warn_dropped`] is the one walker.
pub enum DropWarn {
    /// `control = <name>`, with a fixed message.
    Control(&'static str),
    /// `parameter = <name>`, with the message naming it where the template says `{slot}`.
    Parameter(&'static str),
}

/// One warn per dropped control, in the dialect's words (its `drop_warn`, used by
/// `codec::carry::warn_drops`). Names match `dropped_egress_controls`, so log and seam audit agree.
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
pub fn ir_parse_error() -> busbar_contract::protocol::IrError {
    busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ClientError,
        provider_signal: Some(crate::codec::framework::SIGNAL_IR_PARSE.to_string()),
        retry_after: None,
    }
}

/// Precise context-length prose scan shared by `OpenAiReader::extract_error` and
/// `ResponsesReader::extract_error`. It must be precise: a naive OR of weak tokens
/// (`token`/`maximum`) misclassifies unrelated errors (a quota body like "maximum number of tokens
/// allowed per day" is a rate limit, not oversized). Require a co-located phrase: a canonical
/// self-contained one, or `exceeds` paired with `context`/`token limit`. The caller supplies its own
/// lowercased source (openai scans `error.message`, responses the whole body) and applies the
/// `oversized_status` (400/413) gate itself.
pub fn context_length_prose_scan(text: &str) -> bool {
    text.contains("maximum context length")
        || text.contains("context length exceeded")
        || text.contains("reduce the length")
        || (text.contains("exceeds") && (text.contains("context") || text.contains("token limit")))
}

/// Map an OpenAI-family error `type` to its canonical `code`, shared by the Chat and `/v1/responses`
/// writers (the same envelope). A real bad-key 401 is `{"type":"authentication_error", ...,
/// "code":"invalid_api_key"}` and SDKs surface `error.code`, so `code: null` on an auth (or
/// over-quota) failure is a proxy tell. Every other modeled or passthrough type keeps `null`, as
/// OpenAI does with no machine-readable code. No `_ =>` catch-all: the final arm binds `other`
/// explicitly and emits `null`.
pub fn bearer_error_code(error_type: &str) -> serde_json::Value {
    match error_type {
        ERR_TYPE_AUTHENTICATION => serde_json::Value::String(CODE_INVALID_API_KEY.to_string()),
        // Real OpenAI quota-exhaustion errors carry both `type` and `code` as `insufficient_quota`
        // (HTTP 429). The over-budget governance path (`ingress_error(..., KIND_INSUFFICIENT_QUOTA, ...)`)
        // reaches these writers with that type; `code: null` would mismatch what the SDK surfaces.
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
            // A passthrough type with no modeled code: OpenAI carries none, so `null` is native. Named binding (not `_`).
            let _ = other;
            serde_json::Value::Null
        }
    }
}

/// The OpenAI-style SSE terminator sentinel (`data: [DONE]`). The bare token is matched by the
/// streaming core and several readers; the full framed bytes are emitted on egress. Shared so none re-spells either form.
pub const SSE_DONE_SENTINEL: &str = "[DONE]";
/// The full framed `data: [DONE]\n\n` bytes emitted on egress. See [`SSE_DONE_SENTINEL`].
pub const SSE_DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// Fallback `model` the two bearer-envelope dialects (Chat and Responses) stamp on a cross-protocol
/// answer whose egress supplied none: both schemas make `model` required, so an absent one fails the
/// SDKs' decoders and is a proxy tell. One value so they cannot drift.
pub const FALLBACK_MODEL: &str = "gpt-4o";

/// DoS cap on open tool-call accumulators per stream (OpenAI's parallel-tool-call limit, 128), shared
/// by the Chat and Responses readers' open-item sets and the tool-id remap's memo so none drifts.
pub const MAX_OPEN_TOOL_CALLS: usize = 128;

/// Mixed-case base62 alphabet (digits, lowercase, uppercase; no `-`/`_`) for synthesizing opaque ids
/// for protocols whose native ids are flat random tokens (Gemini `responseId`, Responses
/// `msg_`/`fc_`/`resp_` suffixes), single-sourced so the generators cannot drift. The reject
/// threshold is the largest multiple of 62 in a `u8` (248): a draw in `0..248` maps uniformly via
/// `% 62`; `>= 248` is rejected and redrawn.
pub const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// The rejection-sampling threshold paired with [`BASE62_ALPHABET`].
pub const BASE62_REJECT_THRESHOLD: u8 = 248;

/// Fill `out` (at most 64 characters) with uniform characters of `alphabet` from the host's entropy
/// by rejection sampling: a bare `byte % 62` over-represents residues 0..7 by ~25%, a statistical tell
/// against a native id, so a byte `>= BASE62_REJECT_THRESHOLD` is discarded. Each round draws
/// `out.len()` bytes. `false` on entropy failure, leaving the unfilled tail as it was (callers' `'0'` fill).
pub(crate) fn fill_base62(
    host: &dyn busbar_contract::codec::CodecHost,
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
/// strings, else `None`. A neutral serde_json projection with no protocol knowledge.
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
/// needs only the event type. `""` when there is no `event:` line (OpenAI style) or the name is not
/// UTF-8; the last `event:` line wins if several.
///
/// It walks the same line grammar as [`parse_sse_frame`] / [`busbar_contract::protocol::find_frame_terminator`]
/// (CRLF, a lone LF, or a lone CR each end a line), not LF alone: splitting on LF made a bare-CR
/// frame one line, so the whole body came back as the event name (`message_start\rdata: …`), the
/// Anthropic same-protocol fast path skipped every usage frame of such a stream and billed zero
/// tokens. One grammar keeps the probe and the parse agreeing on where a line ends.
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
/// seam. Carries no dialect knowledge, only the string-passthrough rule.
pub fn tool_arguments_to_string(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::String(s) => s.clone(),
        other => crate::codec::json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// Parse one SSE frame into `(event_type, data_payload)`, both borrowed: the payload is owned only
/// when the frame has several `data:` lines, joined with `\n` per the SSE spec. `event_type` is ""
/// without an `event:` line (OpenAI style). `None` if there is no `data:` line or the frame is invalid UTF-8.
pub fn parse_sse_frame(frame: &[u8]) -> Option<(&str, std::borrow::Cow<'_, str>)> {
    let text = std::str::from_utf8(frame).ok()?;
    let mut event_type = "";
    let mut data: Option<std::borrow::Cow<'_, str>> = None;
    for (start, end) in sse_line_spans(frame) {
        let line = &text[start..end];
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            // Per the SSE spec one leading space after the colon is stripped; the rest is preserved so multi-line JSON survives.
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
/// gives Anthropic-style `event:`/`data:` frames, an empty one OpenAI-style bare `data:`. Writes
/// through the caller's buffer and serializes via `crate::codec::json::to_vec`.
pub fn write_sse_frame(out: &mut Vec<u8>, event_type: &str, data: &serde_json::Value) {
    if !event_type.is_empty() {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event_type.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"data: ");
    // `unwrap_or_default()` matches the decision made a call site up (`stream.rs`): a `Value` that
    // fails to serialise is not a condition this emitter can report.
    out.extend_from_slice(&crate::codec::json::to_vec(data).unwrap_or_default());
    out.extend_from_slice(b"\n\n");
}

// Byte-level top-level `usage` stripper and its two JSON span scanners, used by the OpenAI
// same-protocol verbatim writer. Pure byte scanners.
/// Byte-level removal of a top-level `"usage"` member from a JSON object string, preserving every
/// other byte. `Some(stripped)` when a single top-level `"usage"` was found and removed (with the
/// right adjacent comma); `None` when a safe byte-level edit is not possible (malformed or
/// non-object body, a `"usage"` only nested or inside a string, more than one, or any shape the
/// scanner does not fully understand), and the caller falls back to parse-reserialize for that frame.
///
/// For the same-protocol OpenAI verbatim path: busbar forces `include_usage` upstream to bill, so an
/// OpenAI upstream stamps `"usage":null` on every intermediate chunk, while a native stream for a
/// client that did not request it omits the key. Re-emitting `"usage":null` is a wire-shape tell; this
/// deletes exactly that key without a DOM re-serialize.
///
/// SAFETY: a structural single pass tracking string state (honoring `\`-escapes) and nesting depth, so
/// `"usage"` matches only as a member name at object depth 1, never inside a string value or nested
/// object, and only as a complete quoted string followed (modulo whitespace) by `:`. Anything it
/// cannot classify with certainty yields `None`, never a blind splice.
pub fn strip_top_level_usage_member(json: &str) -> Option<String> {
    let (start, end) = top_level_usage_cut(json)?;
    Some([&json[..start], &json[end..]].concat())
}

/// The byte range [`strip_top_level_usage_member`] removes (the member and exactly one adjacent
/// comma), so a writer can splice around it without building the stripped string.
pub(crate) fn top_level_usage_cut(json: &str) -> Option<(usize, usize)> {
    let bytes = json.as_bytes();
    let n = bytes.len();
    // Skip leading whitespace; the body must be a JSON object.
    let mut i = 0usize;
    while i < n && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= n || bytes[i] != b'{' {
        return None;
    }
    let obj_open = i;
    i += 1;

    // Scan the top-level object's members. `depth` is nesting below the top object (0 is directly
    // inside it); keys are inspected only at depth 0. `member_start` is where the current member
    // begins, so a matched `usage` can be removed with its adjacent comma.
    let mut depth = 0usize;
    // Byte range `[start, end)` of the top-level `usage` member to remove: `start` is the key's
    // opening quote, `end` one past the value.
    let mut usage_range: Option<(usize, usize)> = None;
    // True at the start of a member (after `{` or a top-level `,`): only then is a depth-0 string a key.
    let mut expect_key = true;
    // Whether the scan reached the top-level object's closing `}`. The loop has two exits and only
    // the `depth == 0` break on `}` is well-formed. A truncated object whose members all parsed
    // (`{"a":1,"usage":{"x":1}`, a cut-off upstream chunk, with `usage` on every chunk since busbar
    // forces `include_usage`) produced a `usage_range` and spliced into an unclosed object emitted as
    // valid framing over invalid JSON. "Any shape not fully understood" must give `None`, routing the
    // caller to its parse-reserialize fallback (and, if that fails, the original bytes).
    let mut closed = false;

    while i < n {
        let b = bytes[i];
        match b {
            b'"' => {
                // A string. At depth 0 with `expect_key` it is a member key: capture its span and check
                // for exactly `usage`. Otherwise skip the body.
                let key_start = i;
                let str_end = busbar_contract::spans::string_end(bytes, i)?; // one past the closing quote
                if depth == 0 && expect_key {
                    let is_usage = &bytes[key_start..str_end] == b"\"usage\"";
                    // Advance past the string, then whitespace, then the mandatory `:`.
                    let mut j = str_end;
                    while j < n && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j >= n || bytes[j] != b':' {
                        return None; // not a well-formed member - bail to reserialize
                    }
                    j += 1;
                    // Find the end of this member's value (a full scan respecting nesting/strings).
                    let value_end = busbar_contract::spans::value_end(bytes, j)?;
                    if is_usage {
                        if usage_range.is_some() {
                            return None; // duplicate top-level usage - refuse to guess
                        }
                        usage_range = Some((key_start, value_end));
                    }
                    i = value_end;
                    expect_key = false;
                    continue;
                }
                // A nested string (value or below top level), already fully consumed.
                i = str_end;
            }
            b'{' | b'[' => {
                depth += 1;
                expect_key = false;
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    // Closing the top-level object.
                    if b == b']' {
                        return None; // shape mismatch - top level was not an object after all
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
        return None; // ran off the end of a truncated object - never splice an unclosed shape
    }
    let (start, end) = usage_range?;
    // Remove the member with exactly one adjacent comma so the object stays well-formed: prefer the
    // comma before it (and whitespace between that comma and the key); if it is the first member,
    // take the comma after it. Whitespace around the removed span is trimmed so no dangling `, `
    // remains, matching a native chunk.
    let mut cut_start = start;
    let mut cut_end = end;
    // Look left for a preceding comma (skipping whitespace back to it).
    let mut k = start;
    while k > obj_open + 1 && bytes[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k > obj_open + 1 && bytes[k - 1] == b',' {
        // A preceding comma: remove from it through the member's value.
        cut_start = k - 1;
    } else {
        // `usage` is the first member: remove it through a trailing comma (and its whitespace).
        let mut m = end;
        while m < n && bytes[m].is_ascii_whitespace() {
            m += 1;
        }
        if m < n && bytes[m] == b',' {
            cut_end = m + 1;
        }
        // With no trailing comma either, `usage` was the sole member; removing it leaves a valid `{}`.
    }
    Some((cut_start, cut_end))
}

/// Drop the response-side provider metadata an egress dialect carries and no ingress dialect can
/// express, on the open translate attempt (`codec::drops`), so it does not vanish silently. Which
/// fields exist and the lookup shape are the egress dialect's own, declared on
/// `ProtocolDecl::vendor_response_metadata` (each by wire path) and read here by name so the
/// substrate spells no dialect; `None` reports nothing. Called only from the cross-protocol response
/// seam, so a same-protocol route logs nothing about them.
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
