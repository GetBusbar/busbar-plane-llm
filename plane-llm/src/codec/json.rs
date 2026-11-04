// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors
//
//! The LLM plane's own JSON seam (#83a O6): the one place this plane names its JSON library. Every
//! body parse and serialize on the translate path goes through here, so the implementation (sonic-rs,
//! SIMD on large string-heavy LLM bodies) lives in one place and the in-memory document type is
//! `serde_json::Value`.

use busbar_contract::spans::{exceeds_max_depth, MAX_JSON_DEPTH};

/// Parse body bytes into a document (SIMD). Rejects pathologically nested input (see `MAX_JSON_DEPTH`)
/// before building a `Value`, returning a parse `Err` so callers take their malformed-body (400) path.
#[inline]
pub fn parse<'de, T: serde::Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, sonic_rs::Error> {
    if exceeds_max_depth(bytes, MAX_JSON_DEPTH) {
        return sonic_rs::from_slice::<T>(b"");
    }
    sonic_rs::from_slice(bytes)
}

/// Parse a body `&str` into a document (e.g. an SSE `data:` payload). SIMD-accelerated. Same depth
/// guard as [`parse`].
#[inline]
pub fn parse_str<'de, T: serde::Deserialize<'de>>(s: &'de str) -> Result<T, sonic_rs::Error> {
    if exceeds_max_depth(s.as_bytes(), MAX_JSON_DEPTH) {
        return sonic_rs::from_slice::<T>(b"");
    }
    sonic_rs::from_slice(s.as_bytes())
}

/// Serialize a document to body bytes. SIMD-accelerated; the request/response hot-path serializer.
#[inline]
pub fn to_vec<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, sonic_rs::Error> {
    sonic_rs::to_vec(value)
}

/// Serialize a document to a `String` (error envelopes, SSE-event data). sonic-rs always emits valid
/// UTF-8; its string serializer reuses the output buffer without a second full-body copy.
#[inline]
pub fn to_string<T: serde::Serialize>(value: &T) -> Result<String, sonic_rs::Error> {
    sonic_rs::to_string(value)
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
