// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Observed Responses tool quantities, separate from base-model usage. No prices or invoice rules.
//! `tool_usage.image_gen` is an available-record aggregate, not a complete per-call manifest.
//! https://developers.openai.com/api/reference/resources/responses/methods/retrieve

use crate::codec::ir::{open_class::RESPONSES_TOOL_CLASSES as C, IrUsage};
use busbar_contract::Count;
use serde_json::Value;
use std::collections::BTreeMap;

type Quantities = Vec<(usize, u64)>;
const BAD: &str = "Responses hosted-tool quantity is malformed or inconsistent";

fn number(value: Option<&Value>) -> Result<Option<u64>, &'static str> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let n = value.as_u64().ok_or(BAD)?;
    if !Count::from_micros(i128::from(n) * 1_000_000).fits_a_64_bit_column() {
        return Err(BAD);
    }
    Ok(Some(n))
}
fn object(value: Option<&Value>) -> Result<Option<&serde_json::Map<String, Value>>, &'static str> {
    value
        .filter(|v| !v.is_null())
        .map(|v| v.as_object().ok_or(BAD))
        .transpose()
}
fn pair(value: Option<&Value>, a: &str, b: &str) -> Result<[Option<u64>; 2], &'static str> {
    let v = object(value)?;
    Ok([
        number(v.and_then(|v| v.get(a)))?,
        number(v.and_then(|v| v.get(b)))?,
    ])
}
fn sum(a: u64, b: u64) -> Result<u64, &'static str> {
    a.checked_add(b).ok_or(BAD)
}
fn equal_if_known(total: Option<u64>, a: Option<u64>, b: Option<u64>) -> Result<(), &'static str> {
    if total.is_some_and(|t| a.is_some_and(|a| a > t) || b.is_some_and(|b| b > t)) {
        return Err(BAD);
    }
    if let (Some(t), Some(a), Some(b)) = (total, a, b) {
        if t != sum(a, b)? {
            return Err(BAD);
        }
    }
    Ok(())
}
// A missing scalar can be reconstructed only from both reported modality parts.
// A partial component supplies a lower bound for consistency checking, never an exact total.
fn component_total(
    image: &Value,
    scalar: &str,
    details: &str,
) -> Result<(Option<u64>, u64), &'static str> {
    let total = number(image.get(scalar))?;
    let [a, b] = pair(image.get(details), "text_tokens", "image_tokens")?;
    equal_if_known(total, a, b)?;
    let exact = match (total, a, b) {
        (Some(n), _, _) => Some(n),
        (None, Some(a), Some(b)) => Some(sum(a, b)?),
        _ => None,
    };
    Ok((exact, exact.unwrap_or(sum(a.unwrap_or(0), b.unwrap_or(0))?)))
}
fn partition(
    out: &mut Quantities,
    total: u64,
    cached: Option<u64>,
    first: usize,
) -> Result<(), &'static str> {
    if let Some(cached) = cached {
        out.push((first, total.checked_sub(cached).ok_or(BAD)?));
        out.push((first + 1, cached));
    } else {
        out.push((first + 2, total));
    }
    Ok(())
}
fn input(image: &Value) -> Result<Quantities, &'static str> {
    let total = number(image.get("input_tokens"))?;
    let [text, picture] = pair(
        image.get("input_tokens_details"),
        "text_tokens",
        "image_tokens",
    )?;
    let cached = number(image.get("cached_tokens"))?;
    let [cached_text, cached_image] = pair(
        image.get("cached_tokens_details"),
        "cached_text_tokens",
        "cached_image_tokens",
    )?;
    equal_if_known(total, text, picture)?;
    equal_if_known(cached, cached_text, cached_image)?;
    for (part, cache) in [
        (total, cached),
        (text, cached_text),
        (picture, cached_image),
    ] {
        if let (Some(part), Some(cache)) = (part, cache) {
            if cache > part {
                return Err(BAD);
            }
        }
    }
    let mut out = Vec::new();
    match (text, picture) {
        (Some(text), Some(picture)) => {
            // A scalar cache total with incomplete modality attribution cannot be distributed.
            // Preserve its exact aggregate partition instead of charging inclusive modality rows.
            if cached.is_some() && (cached_text.is_none() || cached_image.is_none()) {
                partition(&mut out, sum(text, picture)?, cached, 6)?;
            } else {
                partition(&mut out, text, cached_text, 0)?;
                partition(&mut out, picture, cached_image, 3)?;
            }
        }
        _ if total.is_some() => {
            let cached = match (cached, cached_text, cached_image) {
                (Some(n), _, _) => Some(n),
                (None, Some(a), Some(b)) => Some(sum(a, b)?),
                _ => None,
            };
            partition(&mut out, total.unwrap(), cached, 6)?;
        }
        _ => {
            if let Some(n) = text {
                partition(&mut out, n, cached_text, 0)?;
            }
            if let Some(n) = picture {
                partition(&mut out, n, cached_image, 3)?;
            }
        }
    }
    Ok(out)
}
fn output(image: &Value) -> Result<Quantities, &'static str> {
    let total = number(image.get("output_tokens"))?;
    let [text, picture] = pair(
        image.get("output_tokens_details"),
        "text_tokens",
        "image_tokens",
    )?;
    equal_if_known(total, text, picture)?;
    Ok(match (text, picture, total) {
        (Some(a), Some(b), _) => vec![(9, a), (10, b)],
        (_, _, Some(n)) => vec![(11, n)],
        (Some(n), None, None) => vec![(9, n)],
        (None, Some(n), None) => vec![(10, n)],
        _ => vec![],
    })
}
fn identity(value: Option<&Value>) -> Result<&str, &'static str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or(BAD)
}
fn calls(response: &Value) -> Result<Quantities, &'static str> {
    let Some(output) = response.get("output") else {
        return Ok(vec![]);
    };
    let output = output.as_array().ok_or(BAD)?;
    let mut seen = BTreeMap::new();
    let mut quantities = [0u64; 3];
    let mut shell_known = true;
    let mut kinds = BTreeMap::new();
    for item in output {
        let kind = item.get("type").and_then(Value::as_str).ok_or(BAD)?;
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            if kinds
                .insert(id, kind)
                .is_some_and(|previous| previous != kind)
                || kinds.len() > 4096
            {
                return Err(BAD);
            }
        }
        let class = match kind {
            "image_generation_call" => 0,
            "code_interpreter_call" => 1,
            "shell_call" => 2,
            _ => continue,
        };
        let id = identity(item.get("id"))?;
        let status = item.get("status").and_then(Value::as_str).ok_or(BAD)?;
        if !matches!(
            status,
            "completed" | "in_progress" | "incomplete" | "failed" | "generating" | "interpreting"
        ) {
            return Err(BAD);
        }
        let container = if class == 1 {
            Some(identity(item.get("container_id"))?)
        } else {
            None
        };
        let environment = if class == 2 {
            object(item.get("environment"))?
        } else {
            None
        };
        let hosted = if class == 2 {
            match environment.and_then(|e| e.get("type")) {
                None | Some(Value::Null) => {
                    shell_known = false;
                    false
                }
                Some(Value::String(kind)) if kind == "local" => false,
                Some(Value::String(kind)) if kind == "container_reference" => true,
                _ => return Err(BAD),
            }
        } else {
            true
        };
        let shell_container = if class == 2 && hosted {
            Some(identity(environment.and_then(|e| e.get("container_id")))?)
        } else {
            None
        };
        let fact = (class, status, container, shell_container, hosted);
        if let Some(previous) = seen.insert(id, fact) {
            if previous != fact {
                return Err(BAD);
            }
            continue;
        }
        if seen.len() > 4096 {
            return Err(BAD);
        }
        if status == "completed" && hosted {
            quantities[class] += 1;
        }
    }
    Ok(quantities
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != 2 || shell_known)
        .map(|(i, n)| (12 + i, n))
        .collect())
}
fn record(ir: &mut IrUsage, result: Result<Quantities, &'static str>) {
    let result = result.and_then(|rows| {
        if rows
            .iter()
            .any(|(_, n)| !Count::from_micros(i128::from(*n) * 1_000_000).fits_a_64_bit_column())
        {
            Err(BAD)
        } else {
            Ok(rows)
        }
    });
    match result {
        Ok(rows) => {
            for (class, n) in rows {
                ir.detail
                    .exact_counts
                    .get_or_insert_with(Default::default)
                    .insert(
                        C[class].into(),
                        Count::from_micros(i128::from(n) * 1_000_000),
                    );
                ir.detail.open_units.insert(C[class].into(), n);
            }
        }
        Err(reason) => {
            if !ir.detail.refused_counts.iter().any(|r| r == reason) {
                ir.detail.refused_counts.push(reason.into());
            }
        }
    }
}
/// Parent usage stays untouched. Every emitted image partition is nonoverlapping, and missing
/// counters are unknown. Totals validate supplied parts but never become additional token rows.
pub(super) fn read(response: Option<&Value>, ir: &mut IrUsage) {
    let Some(response) = response else { return };
    let image = object(response.get("tool_usage"))
        .and_then(|tools| object(tools.and_then(|t| t.get("image_gen"))));
    match image {
        Ok(Some(image)) => {
            let image = Value::Object(image.clone());
            let total_check = (|| {
                let total = number(image.get("total_tokens"))?;
                let (input, input_lower) =
                    component_total(&image, "input_tokens", "input_tokens_details")?;
                let (output, output_lower) =
                    component_total(&image, "output_tokens", "output_tokens_details")?;
                if total.is_some_and(|n| {
                    u128::from(n) < u128::from(input_lower) + u128::from(output_lower)
                }) {
                    return Err(BAD);
                }
                equal_if_known(total, input, output)
            })();
            if let Err(reason) = total_check {
                record(ir, Err(reason));
            } else {
                record(ir, input(&image));
                record(ir, output(&image));
            }
        }
        Ok(None) => {}
        Err(reason) => record(ir, Err(reason)),
    }
    record(ir, calls(response));
}

// Borrow raw component spans so duplicate quantity members cannot acquire last-wins semantics.
// Generated image strings are scanned, never copied by this validation pass.
struct Members<'a>(Vec<(String, &'a serde_json::value::RawValue)>);
impl<'de> serde::Deserialize<'de> for Members<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Members(entries))
            }
        }
        d.deserialize_map(Visitor)
    }
}
/// Only the internal accounting/translation view is masked; original relayed bytes are unchanged.
/// Independent components survive ambiguity in another component.
pub(crate) fn mask_ambiguous(bytes: &[u8], value: &mut Value) {
    let Ok(members) = serde_json::from_slice::<Members<'_>>(bytes) else {
        return;
    };
    let Some(value) = value.as_object_mut() else {
        return;
    };
    for key in ["usage", "tool_usage", "output"] {
        let fields: Vec<_> = members.0.iter().filter(|(name, _)| name == key).collect();
        if fields.len() > 1
            || fields.first().is_some_and(|(_, raw)| {
                crate::codec::json::validate_unique_json(raw.get().as_bytes()).is_err()
            })
        {
            value.insert(key.into(), Value::Bool(false));
        }
    }
}
pub(crate) fn mask_ambiguous_event(bytes: &[u8], value: &mut Value) {
    let Ok(root) = serde_json::from_slice::<Members<'_>>(bytes) else {
        return;
    };
    let responses: Vec<_> = root.0.iter().filter(|(key, _)| key == "response").collect();
    let Some((_, raw)) = responses.first() else {
        return;
    };
    let Some(value) = value.get_mut("response") else {
        return;
    };
    if responses.len() != 1 {
        *value = serde_json::json!({"usage":false,"tool_usage":false,"output":false});
    } else {
        mask_ambiguous(raw.get().as_bytes(), value);
    }
}
