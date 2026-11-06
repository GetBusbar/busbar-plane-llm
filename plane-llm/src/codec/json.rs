// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors
//
//! The LLM plane's own JSON seam (#83a O6): the one place this plane names its JSON library. Every
//! body parse and serialize on the translate path goes through here. Parsing uses serde_json with
//! arbitrary precision so provider counts reach validation without a binary64 round trip. The
//! serializer uses serde_json as well, preserving its arbitrary-precision number representation.

use busbar_contract::spans::{exceeds_max_depth, MAX_JSON_DEPTH};
use serde::{Deserialize, Deserializer};
use serde_json::{value::RawValue, Value};

/// The document forms used by the JSON seam: an exact value or validation without a DOM.
///
/// `Value` cannot use its ordinary `Deserialize` implementation here: with arbitrary precision,
/// it interprets literal objects containing serde_json's private number key as numeric tokens.
pub trait JsonDocument: Sized {
    fn parse_json(bytes: &[u8]) -> Result<Self, serde_json::Error>;
}

impl JsonDocument for Value {
    fn parse_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice::<ExactValue>(bytes).map(|value| value.0)
    }
}

impl JsonDocument for serde::de::IgnoredAny {
    fn parse_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

struct ExactValue(Value);

impl<'de> Deserialize<'de> for ExactValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // RawValue identifies the actual JSON token before any arbitrary-precision transport
        // maps are synthesized. Container members recurse through this same distinction; only
        // scalar tokens reach Value's deserializer. Escaped object keys remain ordinary keys.
        let raw = <&RawValue>::deserialize(deserializer)?.get();
        let value = match raw.as_bytes().first() {
            Some(b'{') => {
                let mut de = serde_json::Deserializer::from_str(raw);
                de.deserialize_map(ObjectVisitor)
            }
            Some(b'[') => serde_json::from_str::<Vec<ExactValue>>(raw)
                .map(|items| Value::Array(items.into_iter().map(|item| item.0).collect())),
            _ => serde_json::from_str::<Value>(raw),
        };
        value.map(ExactValue).map_err(serde::de::Error::custom)
    }
}

struct ObjectVisitor;

impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some((key, value)) = map.next_entry::<String, ExactValue>()? {
            object.insert(key, value.0);
        }
        Ok(Value::Object(object))
    }
}

/// Parse body bytes while retaining exact decimal numbers. Rejects deeply nested input (`MAX_JSON_DEPTH`)
/// before building a `Value`, returning a parse `Err` so callers take their malformed-body (400) path.
#[inline]
pub fn parse<T: JsonDocument>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    if exceeds_max_depth(bytes, MAX_JSON_DEPTH) {
        return T::parse_json(b"");
    }
    T::parse_json(bytes)
}

/// Parse an SSE payload or other string with the same exact-number and depth guarantees as [`parse`].
#[inline]
pub fn parse_str<T: JsonDocument>(s: &str) -> Result<T, serde_json::Error> {
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
