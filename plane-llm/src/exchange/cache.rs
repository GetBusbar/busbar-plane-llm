// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Lane-pinned Responses cache controls and required usage partitions.

use crate::codec::egress_prep::LaneCaps;
use crate::codec::ir::open_class::{CACHE_WRITE_12H_CLASS, CACHE_WRITE_30M_CLASS};
use serde_json::Value;

const OPTIONS: &str = "prompt_cache_options";
const BREAKPOINT: &str = "prompt_cache_breakpoint";
const RETENTION: &str = "prompt_cache_retention";

/// Native relay preserves bytes. Cross-dialect paths refuse these provider controls before
/// clearing unknown IR fields; no TTL is silently translated to a different caching contract.
pub fn validate_request_bytes(
    ingress: &str,
    egress: &str,
    caps: LaneCaps,
    body: Option<&Value>,
    bytes: &[u8],
) -> Result<(), String> {
    if !matches!(ingress, "responses" | "openai") {
        return Ok(());
    }
    let parsed;
    let body = match body {
        Some(value) => value,
        None => {
            // Escaped JSON keys must take the parsed path too; spelling is not authority.
            if !bytes.contains(&b'\\')
                && ![OPTIONS, BREAKPOINT, RETENTION].iter().any(|name| {
                    bytes
                        .windows(name.len())
                        .any(|window| window == name.as_bytes())
                })
            {
                return Ok(());
            }
            parsed = serde_json::from_slice::<Value>(bytes)
                .map_err(|_| "cache controls require a valid JSON request".to_string())?;
            &parsed
        }
    };
    let mut points = Vec::new();
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(point) = item.get(BREAKPOINT) {
            points.push(point);
        }
        for key in ["content", "output"] {
            for block in item
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(point) = block.get(BREAKPOINT) {
                    points.push(point);
                }
            }
        }
    }
    let options = body.get(OPTIONS);
    let retention = body.get(RETENTION);
    if options.is_none() && points.is_empty() && retention.is_none() {
        return Ok(());
    }
    if ingress != egress {
        return Err("prompt cache lifetime controls cannot cross protocol dialects".into());
    }
    if let Some(retention) = retention {
        if caps.responses_cache_ttl {
            return Err(
                "this model requires prompt_cache_options.ttl, not prompt_cache_retention".into(),
            );
        }
        if !matches!(retention.as_str(), Some("in_memory" | "24h")) {
            return Err("prompt_cache_retention must be in_memory or 24h".into());
        }
    }
    if options.is_none() && points.is_empty() {
        return Ok(());
    }
    if ingress != "responses" || !caps.responses_cache_ttl {
        return Err(
            "30m/12h cache controls require responses_cache_ttl on the selected Responses model"
                .into(),
        );
    }
    if let Some(options) = options {
        let object = options
            .as_object()
            .ok_or("prompt_cache_options must be an object")?;
        for (key, value) in object {
            match key.as_str() {
                "ttl" => ttl(value)?,
                "mode" if matches!(value.as_str(), Some("implicit" | "explicit")) => {}
                "prewarm" if value.is_boolean() => {}
                "mode" => {
                    return Err("prompt_cache_options.mode must be implicit or explicit".into())
                }
                "prewarm" => return Err("prompt_cache_options.prewarm must be a boolean".into()),
                _ => return Err("unsupported prompt_cache_options field".into()),
            }
        }
    }
    for point in points {
        let object = point
            .as_object()
            .ok_or("prompt_cache_breakpoint must be an object")?;
        if object.get("mode").and_then(Value::as_str) != Some("explicit") {
            return Err("prompt_cache_breakpoint.mode must be explicit".into());
        }
        for (key, value) in object {
            match key.as_str() {
                "mode" => {}
                "ttl" => ttl(value)?,
                _ => return Err("unsupported prompt_cache_breakpoint field".into()),
            }
        }
    }
    Ok(())
}

fn ttl(value: &Value) -> Result<(), String> {
    if matches!(value.as_str(), Some("30m" | "12h")) {
        Ok(())
    } else {
        Err("prompt cache ttl must be 30m or 12h".into())
    }
}

/// Enforce the selected model's accounting contract on every live reply path, including cuts.
/// The response's model name and the caller's requested lifetime cannot relax this requirement.
pub fn enforce_usage(ctx: &super::reply::ReplyCtx<'_>, piece: &mut super::reply::Piece<'_>) {
    if !ctx.lane.caps.responses_cache_ttl
        || ctx.arrived.operation != busbar_contract::operation::OpVerb::CHAT
    {
        return;
    }
    let units = &mut piece.units;
    let complete = units.exact_counts.as_ref().is_some_and(|counts| {
        counts.contains_key(CACHE_WRITE_30M_CLASS) && counts.contains_key(CACHE_WRITE_12H_CLASS)
    });
    if complete {
        return;
    }
    let reported_usage = units
        .exact_counts
        .as_ref()
        .is_some_and(|counts| !counts.is_empty());
    // No inferred ordinary-input tariff on this model. Output remains independently measurable.
    units.tokens_in = 0;
    units.cache_read = 0;
    units.cache_write = 0;
    units.open.remove("unitemized_tokens");
    if let Some(exact) = &mut units.exact_counts {
        for class in [
            "input",
            "cache_read",
            "cache_write",
            "unitemized_tokens",
            CACHE_WRITE_30M_CLASS,
            CACHE_WRITE_12H_CLASS,
        ] {
            exact.remove(class);
        }
    }
    if reported_usage || (piece.done && piece.verdict == super::reply::Verdict::Ok) {
        let refusal = "selected Responses model requires both 30m/12h cache write counts; input usage is unresolved";
        if !units
            .refused_counts
            .iter()
            .any(|message| message == refusal)
        {
            units.refused_counts.push(refusal.into());
        }
    }
}

#[cfg(test)]
#[path = "../tests/cache_capability.rs"]
mod tests;
