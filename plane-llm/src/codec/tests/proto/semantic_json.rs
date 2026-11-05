// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Exact semantic comparison for translated JSON goldens only. Relay/framing byte tests do not
//! use this comparator. Numbers retain their arbitrary-precision decimal spelling at parse time.

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashSet;

/// Reject duplicate object members before Value parsing could hide a changed or missing member.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Check;
        impl<'de> Visitor<'de> for Check {
            type Value = UniqueKeys;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique object members and exact decimal numbers")
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<UniqueKeys, E> {
                Ok(UniqueKeys)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<UniqueKeys, E> {
                Ok(UniqueKeys)
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<UniqueKeys, E> {
                Ok(UniqueKeys)
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<UniqueKeys, E> {
                Ok(UniqueKeys)
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<UniqueKeys, E> {
                Ok(UniqueKeys)
            }

            // With arbitrary_precision, non-integer numbers arrive through serde_json's exact
            // number map. There is deliberately no binary floating-point visitor here.
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<UniqueKeys, A::Error> {
                while seq.next_element::<UniqueKeys>()?.is_some() {}
                Ok(UniqueKeys)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueKeys, A::Error> {
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON member {key:?}"
                        )));
                    }
                    map.next_value::<UniqueKeys>()?;
                }
                Ok(UniqueKeys)
            }
        }
        deserializer.deserialize_any(Check)
    }
}

fn parse(bytes: &[u8], side: &str) -> Result<Value, String> {
    serde_json::from_slice::<UniqueKeys>(bytes)
        .map_err(|e| format!("{side} JSON cannot be classified: {e}"))?;
    // Value's arbitrary-precision deserializer reserves this object key as its numeric transport.
    // Reject a literal wire object using it rather than confuse an object/string with a number.
    // The preceding parse validated the grammar; this walk only identifies string tokens used as
    // keys, including escaped spellings, and never interprets a numeric token.
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while bytes[i] != b'"' {
            i += if bytes[i] == b'\\' { 2 } else { 1 };
        }
        i += 1;
        let end = i;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) == Some(&b':') {
            let key: String = serde_json::from_slice(&bytes[start..end])
                .map_err(|e| format!("{side} JSON cannot be classified: {e}"))?;
            if key == "$serde_json::private::Number" {
                return Err(format!(
                    "{side} JSON cannot be classified: reserved numeric marker used as object key"
                ));
            }
        }
    }
    serde_json::from_slice(bytes).map_err(|e| format!("{side} JSON cannot be classified: {e}"))
}

/// A nonzero number is sign × coefficient × 10^exponent. Leading and trailing coefficient zeros
/// are removed without constructing an expanded decimal or narrowing its significant digits.
fn decimal(raw: &str) -> Result<(bool, String, i128), &'static str> {
    let negative = raw.starts_with('-');
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, power) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let fraction = mantissa.split_once('.').map_or(0, |(_, f)| f.len());
    let digits = mantissa.replace('.', "");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Ok((false, "0".to_owned(), 0));
    }
    let coefficient = significant.trim_end_matches('0');
    let removed = significant.len() - coefficient.len();
    let power = power.parse::<i128>().map_err(|_| "exponent exceeds i128")?;
    let fraction = i128::try_from(fraction).map_err(|_| "fraction length exceeds i128")?;
    let removed = i128::try_from(removed).map_err(|_| "coefficient length exceeds i128")?;
    let exponent = power
        .checked_sub(fraction)
        .and_then(|p| p.checked_add(removed))
        .ok_or("normalized exponent exceeds i128")?;
    Ok((negative, coefficient.to_owned(), exponent))
}

fn numbers(expected: &str, actual: &str, path: &str) -> Result<(), String> {
    // Identical decimal tokens prove equality even when their exponent exceeds our arithmetic.
    if expected == actual {
        return Ok(());
    }
    let normalize = |raw| {
        decimal(raw).map_err(|reason| {
            format!("numeric equality unclassified at {path}: {reason}; {expected} vs {actual}")
        })
    };
    if normalize(expected)? == normalize(actual)? {
        Ok(())
    } else {
        Err(format!(
            "numeric value changed at {path}: {expected} vs {actual}"
        ))
    }
}

fn values(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    match (expected, actual) {
        (Value::Number(a), Value::Number(b)) => numbers(&a.to_string(), &b.to_string(), path),
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Err(format!("array length changed at {path}"));
            }
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                values(a, b, &format!("{path}/{i}"))?;
            }
            Ok(())
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.len() != b.len() {
                return Err(format!("object member count changed at {path}"));
            }
            for (key, value) in a {
                let next = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                let other = b
                    .get(key)
                    .ok_or_else(|| format!("required member missing at {next}"))?;
                values(value, other, &next)?;
            }
            Ok(())
        }
        _ if expected == actual => Ok(()),
        _ => Err(format!("JSON type or value changed at {path}")),
    }
}

pub(super) fn compare(expected: &[u8], actual: &[u8]) -> Result<(), String> {
    values(&parse(expected, "golden")?, &parse(actual, "actual")?, "$")
}

#[test]
fn cosmetic_json_changes_preserve_exact_values() {
    for (expected, actual) in [
        (
            r#"{"n":27,"text":"a","items":[1,2]}"#,
            r#" { "items": [1.0,2e0], "text":"\u0061", "n":27.0 } "#,
        ),
        ("1000", "1e3"),
        ("-0.00", "0e999999999999999999999999999999999999999999"),
        ("9007199254740993", "9007199254740993.0"),
        ("1e400", "10e399"),
    ] {
        assert_eq!(compare(expected.as_bytes(), actual.as_bytes()), Ok(()));
    }
}

#[test]
fn precision_loss_and_semantic_changes_are_rejected() {
    for (expected, actual) in [
        ("9007199254740992", "9007199254740993"),
        ("27", "27.0000000000000001"),
        ("1e400", "1e401"),
        ("[1,2]", "[2,1]"),
        (r#""27""#, "27"),
        (r#""a ""#, r#""a""#),
        (r#"{"required":null}"#, "{}"),
        (r#"{"required":1}"#, r#"{"other":1}"#),
        ("true", "1"),
    ] {
        assert!(compare(expected.as_bytes(), actual.as_bytes()).is_err());
    }
}

#[test]
fn extreme_exponents_require_proven_equality() {
    let huge = "1e999999999999999999999999999999999999999999";
    assert_eq!(compare(huge.as_bytes(), huge.as_bytes()), Ok(()));
    let error = compare(
        huge.as_bytes(),
        b"10e999999999999999999999999999999999999999998",
    )
    .unwrap_err();
    assert!(error.contains("numeric equality unclassified"), "{error}");
}

#[test]
fn invalid_json_and_duplicate_members_cannot_hide_drift() {
    for actual in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"a":1,"\u0061":2}"#,
        br#"{"a":[{"n":1,"n":2}]}"#,
        br#"{"$serde_json::private::Number":"27"}"#,
        br#"{"\u0024serde_json::private::Number":"27"}"#,
        b"{",
    ] {
        let error = compare(br#"{"a":2}"#, actual).unwrap_err();
        assert!(error.contains("cannot be classified"), "{error}");
    }
}
