// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Anthropic `usage` objects: the usage table and the buffered / `message_delta` usage writers.

use super::IrError;
use crate::codec::keys;
use crate::codec::usage_count::{read_usage, CountRead, CountSlot, UsageCount};

/// Anthropic's usage counts, as data. The four totals come first, then the 5m/1h cache-creation

pub(super) const USAGE: &[UsageCount] = &[
    (CountSlot::Input, CountRead::Zero(&[keys::INPUT_TOKENS])),
    (CountSlot::Output, CountRead::Zero(&[keys::OUTPUT_TOKENS])),
    (
        CountSlot::CacheWrite,
        CountRead::Opt(&[super::CACHE_CREATION_INPUT_TOKENS]),
    ),
    (
        CountSlot::CacheRead,
        CountRead::Opt(&[super::CACHE_READ_INPUT_TOKENS]),
    ),
    (
        CountSlot::CacheWrite5m,
        CountRead::Opt(&[super::CACHE_CREATION, super::EPHEMERAL_5M_INPUT_TOKENS]),
    ),
    (
        CountSlot::CacheWrite1h,
        CountRead::Opt(&[super::CACHE_CREATION, super::EPHEMERAL_1H_INPUT_TOKENS]),
    ),
    (
        CountSlot::WebSearchRequests,
        CountRead::Opt(&[super::SERVER_TOOL_USE, super::WEB_SEARCH_REQUESTS]),
    ),
    (
        CountSlot::WebFetchRequests,
        CountRead::Opt(&[super::SERVER_TOOL_USE, super::WEB_FETCH_REQUESTS]),
    ),
    (
        CountSlot::Reasoning,
        CountRead::Lenient(&[keys::OUTPUT_TOKENS_DETAILS, super::THINKING_TOKENS]),
    ),
];

/// This dialect's label on a refused usage count.
pub(super) const COUNT_LABEL: &str = super::VENDOR_NAME;

/// A wire `usage` object (`None` when the frame carries none) to the IR usage: the [`USAGE`] table

pub(super) fn read_anthropic_usage(
    usage_val: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    let mut usage = read_usage(COUNT_LABEL, usage_val, USAGE)?;
    usage.detail.service_tier = usage_val
        .and_then(|u| u.get(keys::SERVICE_TIER))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(usage)
}

/// The `cache_creation` tier object `{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`; the

pub(super) fn write_cache_creation_object(usage: &crate::codec::ir::IrUsage) -> serde_json::Value {
    let d = &usage.detail;
    let any_tier =
        d.cache_creation_5m_input_tokens.is_some() || d.cache_creation_1h_input_tokens.is_some();
    if !any_tier && usage.cache_creation_input_tokens.unwrap_or(0) > 0 {
        return serde_json::Value::Null;
    }
    serde_json::json!({
        (super::EPHEMERAL_5M_INPUT_TOKENS): d.cache_creation_5m_input_tokens.unwrap_or(0),
        (super::EPHEMERAL_1H_INPUT_TOKENS): d.cache_creation_1h_input_tokens.unwrap_or(0),
    })
}

/// `usage.output_tokens_details`: `{thinking_tokens: N}` when known, else `null`. Required by both

pub(super) fn write_output_tokens_details(usage: &crate::codec::ir::IrUsage) -> serde_json::Value {
    match usage.detail.reasoning_tokens {
        Some(t) => serde_json::json!({ (super::THINKING_TOKENS): t }),
        None => serde_json::Value::Null,
    }
}

/// `usage.server_tool_use`: `{web_search_requests, web_fetch_requests}` when a server-tool count is

pub(super) fn write_server_tool_use(usage: &crate::codec::ir::IrUsage) -> serde_json::Value {
    match usage.detail.web_search_requests {
        Some(n) => {
            serde_json::json!({ (super::WEB_SEARCH_REQUESTS): n, (super::WEB_FETCH_REQUESTS): 0 })
        }
        None => serde_json::Value::Null,
    }
}

/// The full wire `usage` object of a `Message` (the buffered response and the

pub(super) fn write_usage_object(usage: Option<&crate::codec::ir::IrUsage>) -> serde_json::Value {
    let zero = crate::codec::ir::IrUsage::default();
    let usage = usage.unwrap_or(&zero);
    let mut usage_map = serde_json::Map::new();
    usage_map.insert(
        keys::INPUT_TOKENS.to_string(),
        serde_json::json!(usage.input_tokens),
    );
    usage_map.insert(
        keys::OUTPUT_TOKENS.to_string(),
        serde_json::json!(usage.output_tokens),
    );
    usage_map.insert(
        super::CACHE_CREATION_INPUT_TOKENS.to_string(),
        serde_json::json!(usage.cache_creation_input_tokens.unwrap_or(0)),
    );
    usage_map.insert(
        super::CACHE_READ_INPUT_TOKENS.to_string(),
        serde_json::json!(usage.cache_read_input_tokens.unwrap_or(0)),
    );
    usage_map.insert(
        super::CACHE_CREATION.to_string(),
        write_cache_creation_object(usage),
    );
    usage_map.insert("inference_geo".to_string(), serde_json::Value::Null);
    usage_map.insert(
        keys::OUTPUT_TOKENS_DETAILS.to_string(),
        write_output_tokens_details(usage),
    );
    usage_map.insert(
        super::SERVER_TOOL_USE.to_string(),
        write_server_tool_use(usage),
    );

    let tier = match usage.detail.service_tier.as_deref() {
        None => serde_json::json!(super::STANDARD),
        reported => anthropic_served_tier(reported)
            .map_or(serde_json::Value::Null, |t| serde_json::json!(t)),
    };
    usage_map.insert(keys::SERVICE_TIER.to_string(), tier);
    serde_json::Value::Object(usage_map)
}

/// The IR served tier to a `usage.service_tier` word, or `None` when Anthropic has none. The IR

pub(super) fn anthropic_served_tier(tier: Option<&str>) -> Option<&'static str> {
    let word = tier?;
    let mapped = match word {
        super::STANDARD | "default" => Some(super::STANDARD),
        super::PRIORITY => Some(super::PRIORITY),
        super::BATCH => Some(super::BATCH),
        _ => None,
    };
    if mapped.is_none() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::SERVICE_TIER,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [service_tier = word,],
            "dropping usage.service_tier on Anthropic response egress: Anthropic names only \
             standard / priority / batch"
        );
    }
    mapped
}

/// The wire `usage` object of a `message_delta` (`MessageDeltaUsage`). `input_tokens`,

pub(super) fn write_message_delta_usage(usage: &crate::codec::ir::IrUsage) -> serde_json::Value {
    let mut usage_map = serde_json::Map::new();
    usage_map.insert(
        keys::INPUT_TOKENS.to_string(),
        serde_json::json!(usage.input_tokens),
    );
    usage_map.insert(
        keys::OUTPUT_TOKENS.to_string(),
        serde_json::json!(usage.output_tokens),
    );
    usage_map.insert(
        super::CACHE_CREATION_INPUT_TOKENS.to_string(),
        serde_json::json!(usage.cache_creation_input_tokens.unwrap_or(0)),
    );
    usage_map.insert(
        super::CACHE_READ_INPUT_TOKENS.to_string(),
        serde_json::json!(usage.cache_read_input_tokens.unwrap_or(0)),
    );
    usage_map.insert(
        keys::OUTPUT_TOKENS_DETAILS.to_string(),
        write_output_tokens_details(usage),
    );
    usage_map.insert(
        super::SERVER_TOOL_USE.to_string(),
        write_server_tool_use(usage),
    );
    let d = &usage.detail;
    if d.cache_creation_5m_input_tokens.is_some() || d.cache_creation_1h_input_tokens.is_some() {
        usage_map.insert(
            super::CACHE_CREATION.to_string(),
            write_cache_creation_object(usage),
        );
    }
    if let Some(tier) = anthropic_served_tier(d.service_tier.as_deref()) {
        usage_map.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
    }
    serde_json::Value::Object(usage_map)
}
