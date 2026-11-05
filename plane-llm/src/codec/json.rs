// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors
//
//! The LLM plane's own JSON seam (#83a O6): the one place this plane names its JSON library. Every
//! body parse and serialize on the translate path goes through here. Parsing uses serde_json with
//! arbitrary precision so provider counts reach validation without a binary64 round trip. The
//! serializer uses serde_json as well, preserving its arbitrary-precision number representation.

use busbar_contract::spans::{exceeds_max_depth, MAX_JSON_DEPTH};

/// Parse body bytes while retaining exact decimal numbers. Rejects deeply nested input (`MAX_JSON_DEPTH`)
/// before building a `Value`, returning a parse `Err` so callers take their malformed-body (400) path.
#[inline]
pub fn parse<'de, T: serde::Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, serde_json::Error> {
    if exceeds_max_depth(bytes, MAX_JSON_DEPTH) {
        return serde_json::from_slice::<T>(b"");
    }
    serde_json::from_slice(bytes)
}

/// Parse an SSE payload or other string with the same exact-number and depth guarantees as [`parse`].
#[inline]
pub fn parse_str<'de, T: serde::Deserialize<'de>>(s: &'de str) -> Result<T, serde_json::Error> {
    parse(s.as_bytes())
}

/// Serialize a document to body bytes, preserving arbitrary-precision JSON numbers.
#[inline]
pub fn to_vec<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

/// Serialize a document to a `String` (error envelopes, SSE-event data) with exact numbers.
#[inline]
pub fn to_string<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// Sanitized one-line parse-error description for operator logs, never the raw library `Display`,
/// which (with sonic-rs) embeds a fragment of the offending bytes that may hold secrets/PII. "<n>
/// bytes" suffices to correlate.
#[inline]
pub fn parse_err_log(bytes_len: usize) -> String {
    format!("invalid JSON ({bytes_len} bytes)")
}

#[cfg(test)]
#[path = "tests/json_seam_tests.rs"]
mod seam_tests;
