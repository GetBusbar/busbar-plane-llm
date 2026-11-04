// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Anthropic's native error envelope. (Its egress credential is not built here: the dialect
//! DECLARES its scheme on `DECL` and the kernel presents the lane credential under it.)

use super::*;
use crate::codec::keys;

impl AnthropicWriter {
    /// Build the native error envelope for a resolved `error.type`. Native bodies carry a top-level
    /// `request_id`; busbar mints one since its absence is a tell.
    pub(super) fn error_envelope(
        host: &dyn crate::codec::cells::CodecHost,
        error_type: &str,
        message: &str,
    ) -> serde_json::Value {
        let mut envelope = Self::error_envelope_with_request_id(error_type, message, "");
        envelope[super::REQUEST_ID] = synth_request_id(host)
            .ok()
            .map_or(serde_json::Value::Null, serde_json::Value::String);
        envelope
    }

    /// The same envelope with the `request_id` supplied (see [`request_id_from_entropy`]), for a
    /// caller that may not read a random source.
    pub fn error_envelope_with_request_id(
        error_type: &str,
        message: &str,
        request_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            (keys::TYPE): keys::ERROR_WORD,
            (keys::ERROR_WORD): {
                (keys::TYPE): error_type,
                (keys::MESSAGE): message,
            },
            (super::REQUEST_ID): request_id,
        })
    }
}
