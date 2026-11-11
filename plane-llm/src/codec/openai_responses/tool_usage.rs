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
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellEnvironment {
    Local,
    Container(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallFact {
    kind: usize,
    container: Option<String>,
    environment: Option<ShellEnvironment>,
    final_status: Option<String>,
}
fn call_fact(item: &Value, final_item: bool) -> Result<Option<(String, CallFact)>, &'static str> {
    let kind = match item.get("type").and_then(Value::as_str).ok_or(BAD)? {
        "image_generation_call" => 0,
        "code_interpreter_call" => 1,
        "shell_call" => 2,
        _ => return Ok(None),
    };
    let id = identity(item.get("id"))?.to_owned();
    let status = item.get("status").and_then(Value::as_str).ok_or(BAD)?;
    if !matches!(
        status,
        "completed" | "in_progress" | "incomplete" | "failed" | "generating" | "interpreting"
    ) {
        return Err(BAD);
    }
    let container = if kind == 1 {
        Some(identity(item.get("container_id"))?.to_owned())
    } else {
        None
    };
    let environment = if kind == 2 {
        let env = object(item.get("environment"))?;
        match env.and_then(|e| e.get("type")) {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) if kind == "local" => Some(ShellEnvironment::Local),
            Some(Value::String(kind)) if kind == "container_reference" => {
                Some(ShellEnvironment::Container(
                    identity(env.and_then(|e| e.get("container_id")))?.to_owned(),
                ))
            }
            _ => return Err(BAD),
        }
    } else {
        None
    };
    Ok(Some((
        id,
        CallFact {
            kind,
            container,
            environment,
            final_status: final_item.then(|| status.to_owned()),
        },
    )))
}
fn call_rows(items: &BTreeMap<String, CallFact>, snapshot: bool) -> Quantities {
    let mut counts = [0u64; 3];
    let mut unknown = [false; 3];
    for item in items.values() {
        if item.kind == 2 && item.environment.is_none() {
            unknown[2] = true;
        }
        if item.final_status.as_deref() != Some("completed") {
            continue;
        }
        match item.kind {
            0 => counts[0] += 1,
            1 if item.container.is_some() => counts[1] += 1,
            1 => unknown[1] = true,
            2 if matches!(item.environment, Some(ShellEnvironment::Container(_))) => counts[2] += 1,
            _ => {}
        }
    }
    counts
        .into_iter()
        .enumerate()
        .filter(|(i, n)| {
            !unknown[*i] && (*n > 0 || (snapshot && items.values().any(|item| item.kind == *i)))
        })
        .map(|(i, n)| (12 + i, n))
        .collect()
}
fn snapshot_calls(response: &Value) -> Result<Option<BTreeMap<String, CallFact>>, &'static str> {
    let Some(output) = response.get("output") else {
        return Ok(None);
    };
    let Some(output) = output.as_array() else {
        return Ok(None);
    };
    let mut items = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for item in output {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            if kinds
                .insert(id, kind)
                .is_some_and(|previous| previous != kind)
                || kinds.len() > 4096
            {
                return Err(BAD);
            }
        }
        if let Some((id, fact)) = call_fact(item, true)? {
            if items
                .insert(id, fact.clone())
                .is_some_and(|previous| previous != fact)
                || items.len() > 4096
            {
                return Err(BAD);
            }
        }
    }
    Ok(Some(items))
}
fn calls(response: &Value) -> Result<Quantities, &'static str> {
    Ok(snapshot_calls(response)?.map_or_else(Vec::new, |items| call_rows(&items, true)))
}

/// Bounded call metadata observed on one provider stream. Payloads are never retained.
/// A completed event can survive a cut stream, and a final snapshot refines the same identities.
#[derive(Debug, Clone, Default)]
pub(crate) struct StreamCalls {
    items: BTreeMap<String, CallFact>,
    indices: BTreeMap<u64, String>,
    snapshot: bool,
    refused: bool,
}
impl StreamCalls {
    fn insert(&mut self, id: String, fact: CallFact) -> Result<(), &'static str> {
        if let Some(previous) = self.items.get_mut(&id) {
            fn refine<T: PartialEq>(
                old: &mut Option<T>,
                new: Option<T>,
            ) -> Result<(), &'static str> {
                if let Some(new) = new {
                    if old.as_ref().is_some_and(|old| old != &new) {
                        return Err(BAD);
                    }
                    *old = Some(new);
                }
                Ok(())
            }
            if previous.kind != fact.kind {
                return Err(BAD);
            }
            refine(&mut previous.container, fact.container)?;
            refine(&mut previous.environment, fact.environment)?;
            refine(&mut previous.final_status, fact.final_status)?;
        } else {
            if self.items.len() == 4096 {
                return Err(BAD);
            }
            self.items.insert(id, fact);
        }
        Ok(())
    }
    fn bind_index(&mut self, index: u64, id: &str) -> Result<(), &'static str> {
        if index >= 4096 {
            return Err(BAD);
        }
        if let Some(previous) = self.indices.get(&index) {
            if previous != id {
                return Err(BAD);
            }
        } else {
            self.indices.insert(index, id.to_owned());
        }
        Ok(())
    }
    fn event(&mut self, event: &str, data: &Value) -> Result<(), &'static str> {
        match event {
            "response.output_item.added" | "response.output_item.done" => {
                let index = event_index(data)?;
                if let Some((id, fact)) =
                    call_fact(data.get("item").ok_or(BAD)?, event.ends_with(".done"))?
                {
                    self.bind_index(index, &id)?;
                    self.insert(id, fact)?;
                }
            }
            "response.image_generation_call.completed"
            | "response.code_interpreter_call.completed" => {
                let index = event_index(data)?;
                let id = identity(data.get("item_id"))?.to_owned();
                self.bind_index(index, &id)?;
                self.insert(
                    id,
                    CallFact {
                        kind: usize::from(event == "response.code_interpreter_call.completed"),
                        container: None,
                        environment: None,
                        final_status: Some("completed".into()),
                    },
                )?;
            }
            _ => {
                let response = data.get("response").ok_or(BAD)?;
                if let Some(items) = snapshot_calls(response)? {
                    for (index, item) in response
                        .get("output")
                        .and_then(Value::as_array)
                        .ok_or(BAD)?
                        .iter()
                        .enumerate()
                    {
                        if let Some(id) = item
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| items.contains_key(*id))
                        {
                            self.bind_index(index as u64, id)?;
                        }
                    }
                    for (id, fact) in items {
                        self.insert(id, fact)?;
                    }
                    self.snapshot = true;
                }
            }
        }
        Ok(())
    }
    /// Return whether this event carries call evidence; malformed relevant evidence is sticky.
    pub(crate) fn observe(&mut self, event: &str, data: &Value) -> bool {
        if !matches!(
            event,
            "response.output_item.added"
                | "response.output_item.done"
                | "response.image_generation_call.completed"
                | "response.code_interpreter_call.completed"
                | "response.completed"
                | "response.incomplete"
                | "response.failed"
        ) {
            return false;
        }
        if !self.refused && self.event(event, data).is_err() {
            self.refused = true;
        }
        true
    }
    /// Replace only call classes. This also removes an earlier plausible count after contradictory
    /// evidence, while independent parent and image-token observations remain available.
    pub(crate) fn apply(&self, usage: &mut IrUsage) {
        let exact = usage
            .detail
            .exact_counts
            .get_or_insert_with(Default::default);
        for name in &C[12..] {
            exact.remove(*name);
            usage.detail.open_units.remove(*name);
        }
        record(
            usage,
            if self.refused {
                Err(BAD)
            } else {
                Ok(call_rows(&self.items, self.snapshot))
            },
        );
    }
}
fn event_index(data: &Value) -> Result<u64, &'static str> {
    let index = data
        .get("output_index")
        .and_then(Value::as_u64)
        .filter(|i| *i < 4096)
        .ok_or(BAD)?;
    if data
        .get("sequence_number")
        .is_some_and(|n| n.as_u64().is_none())
    {
        return Err(BAD);
    }
    Ok(index)
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
    if let Some(value) = value.as_object_mut() {
        for key in ["item", "item_id", "output_index", "sequence_number"] {
            let fields: Vec<_> = root.0.iter().filter(|(name, _)| name == key).collect();
            if fields.len() > 1
                || fields.first().is_some_and(|(_, raw)| {
                    crate::codec::json::validate_unique_json(raw.get().as_bytes()).is_err()
                })
            {
                value.insert(key.into(), Value::Bool(false));
            }
        }
    }
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
