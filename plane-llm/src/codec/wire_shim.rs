// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Body-shaping helpers on either side of a translate. They are pure over the plane's own
//! declarations and the neutral billing carrier (no socket, clock or configuration but the host's
//! translation cap), and the suites that pin them are codec suites, so they travel with the codecs.

use serde_json::Value;

/// Remove the router-internal shim keys the route layer injects for path-model ingress protocols
/// (`gemini`, `bedrock`), whose wire carries the model in the URL and stream intent in the path.
///
///   - The gemini JSON-array key is never a native body field for any backend (it only affects
///     response framing), so it is stripped unconditionally, for body-model ingress too.
///   - `stream` is a body field for body-model protocols (openai/anthropic/cohere/responses), where
///     the egress writer authoritatively writes `"stream": <ir.stream>`, and a path shim for
///     path-model egress (`gemini`/`bedrock`). So it is stripped iff the egress is gemini/bedrock,
///     never by ingress: an ingress-gated strip would delete the writer-authored `"stream": true` on a
///     path-model-ingress to body-model-egress streaming hop, and the backend would answer non-streaming.
///   - `model` is never stripped here: a body-model egress requires it and `rewrite_model` installs
///     the authoritative one. Within one dialect the relay removes the spliced `model`.
///
/// Returns whether the body changed. It runs on a translated body only; within one dialect the relay
/// removes the same members by byte splices (`exchange::attempt::relay_request`).
pub fn strip_router_shim_keys(v: &mut Value, egress_protocol: &str) -> bool {
    let mut changed = false;
    if let Some(obj) = v.as_object_mut() {
        // A protocol's array-stream shim key is never native to any backend wire: strip every key this
        // plane's dialects declare, unconditionally (also closing the leak of a body-model client
        // smuggling one). Reading the declarations names no shim-key literal; the six-entry table is
        // this plane's own (Law 5), so the walk allocates nothing. `remove` returning the old value
        // means a real mutation (#1).
        for key in crate::codec::DECLS
            .iter()
            .filter_map(|d| d.array_stream_shim_key)
        {
            if obj.remove(key).is_some() {
                changed = true;
            }
        }
        // `stream` is a path-model shim for gemini/bedrock egress (stream intent and model ride the
        // URL; `has_model_in_url()` covers both). For body-model egress it is the writer-authored
        // field the backend needs, so it is preserved. Gate on egress, never ingress.
        if crate::codec::decl_of(egress_protocol)
            .map(|d| d.has_model_in_url)
            .unwrap_or(false)
            && obj.remove("stream").is_some()
        {
            // #2: `stream` present and the egress is path-model: a real mutation.
            changed = true;
        }
    }
    changed
}

/// Bytes-per-token divisor for the truncated-tail billing floor. Conservative (~4 bytes/token is
/// typical English prose; a JSON envelope with field names and escaping runs higher), so it
/// under-counts: the retained tail is only the last `cap` bytes of a larger body, making this a floor
/// that cannot over-charge. Its only job is to keep a truncated-beyond-recovery response from billing zero.
pub const TRUNCATED_TAIL_BYTES_PER_TOKEN: u64 = 4;

#[cfg(test)]
#[path = "tests/wire_shim_tests.rs"]
mod tests;
