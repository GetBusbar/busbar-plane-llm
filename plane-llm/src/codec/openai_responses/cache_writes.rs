// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Responses cache write attribution. A reported split is an economic partition, not a hint.

use crate::codec::ir::{
    open_class::{CACHE_WRITE_12H_CLASS, CACHE_WRITE_30M_CLASS},
    IrUsage,
};
use crate::codec::usage_count::exact_count_opt;
use busbar_contract::Count;
use serde_json::Value;

pub(super) fn normalize(wire: Option<&Value>, usage: &mut IrUsage) {
    let details = wire.and_then(|v| v.get(super::INPUT_TOKENS_DETAILS));
    // Legacy models retain their aggregate write semantics. A present partition, including a
    // malformed/null partition, must never silently fall back to that legacy tariff.
    let Some(split) = details.and_then(|d| d.get("cache_write_tokens_details")) else {
        return;
    };
    let exact = usage
        .detail
        .exact_counts
        .as_mut()
        .expect("common usage reader supplies exact counts");
    let partition = (|| {
        if !split.is_object() {
            return None;
        }
        let a = *exact.get(CACHE_WRITE_30M_CLASS)?;
        let b = *exact.get(CACHE_WRITE_12H_CLASS)?;
        let writes = *exact.get("cache_write")?;
        let input = exact_count_opt(wire, "input_tokens").ok()??;
        // Missing reads are not authoritative evidence for a disjoint partition.
        let reads = *exact.get("cache_read")?;
        let sum = a.checked_add(b).ok()?;
        if sum != writes || !sum.fits_a_64_bit_column() {
            return None;
        }
        let ordinary = input.checked_sub(reads).ok()?.checked_sub(writes).ok()?;
        if ordinary < Count::ZERO || !ordinary.fits_a_64_bit_column() {
            return None;
        }
        Some(ordinary)
    })();
    match partition {
        Some(ordinary) => {
            exact.insert("input".into(), ordinary);
            exact.remove("cache_write");
        }
        None => {
            // Keep output independent; no cache/input amount is authoritative if the partition
            // is contradictory. Refused usage stays unresolved through the normal driver path.
            for class in [
                "input",
                "cache_read",
                "cache_write",
                CACHE_WRITE_30M_CLASS,
                CACHE_WRITE_12H_CLASS,
            ] {
                exact.remove(class);
            }
            usage.input_tokens = 0;
            usage.cache_read_input_tokens = None;
            usage.cache_creation_input_tokens = None;
            usage.detail.cache_creation_30m_input_tokens = None;
            usage.detail.cache_creation_12h_input_tokens = None;
            usage.detail.refused_counts.push(
                "Responses cache write partition is missing, invalid, or inconsistent".into(),
            );
        }
    }
}
