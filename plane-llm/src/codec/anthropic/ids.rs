// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Synthesized native-format Anthropic ids (`msg_…`, `req_…`) drawn from the OS CSPRNG.

use super::*;

/// Mint a native-shaped `msg_01` id for the cross-protocol path, where the backend supplied none.
pub(super) fn synth_message_id(
    host: &dyn crate::codec::cells::CodecHost,
) -> Result<String, IrError> {
    synth_id_with_prefix(host, "msg_")
}

/// Mint a native-shaped `req_01` id for the top level of a synthesized error envelope, which
/// carries a `request_id` on the native API.
pub(super) fn synth_request_id(
    host: &dyn crate::codec::cells::CodecHost,
) -> Result<String, IrError> {
    synth_id_with_prefix(host, "req_")
}

/// The same `req_01<24 base62>` id built from entropy the caller supplies (a plane may read no
/// random source; it is handed one).
#[must_use]
pub fn request_id_from_entropy(entropy: &[u8]) -> Option<String> {
    const BASE62_REJECT_FLOOR: u8 = crate::codec::dialect::BASE62_REJECT_THRESHOLD;
    let alphabet = ANTHROPIC_NATIVE_ALPHABET;
    let mut token = [b'0'; SYNTH_ID_TOKEN_LEN];
    let mut filled = 0usize;
    if !entropy.is_empty() {
        let mut rounds = 0usize;
        while filled < SYNTH_ID_TOKEN_LEN && rounds < SYNTH_ID_TOKEN_LEN + 1 {
            let mut progressed = false;
            for &byte in entropy {
                if byte >= BASE62_REJECT_FLOOR {
                    continue;
                }
                token[filled] = alphabet[(byte % 62) as usize];
                filled += 1;
                progressed = true;
                if filled == SYNTH_ID_TOKEN_LEN {
                    break;
                }
            }
            if !progressed {
                break;
            }
            rounds += 1;
        }
    }
    if filled != SYNTH_ID_TOKEN_LEN {
        return None;
    }

    let token = std::str::from_utf8(&token).ok()?;
    Some(format!("req_01{token}"))
}

/// Shared construction for `msg_` and `req_`: the `01` marker plus a 24-char token drawn entirely
/// from the OS CSPRNG, with no counter or clock (those leave predictable characters, a structural
/// tell).
pub(super) fn synth_id_with_prefix(
    host: &dyn crate::codec::cells::CodecHost,
    prefix: &str,
) -> Result<String, IrError> {
    let mut token = [b'0'; SYNTH_ID_TOKEN_LEN];
    if !crate::codec::dialect::fill_base62(host, &mut token, ANTHROPIC_NATIVE_ALPHABET) {
        return Err(entropy_error());
    }
    let token = std::str::from_utf8(&token).map_err(|_| entropy_error())?;
    Ok(format!("{prefix}01{token}"))
}

/// Mint a request id (`req_01` + 24 base62 chars, 30 in all, matching the body `request_id`) for
/// the `request-id` response header a native response always carries; omitting it is a proxy tell.
pub fn synth_anthropic_request_id(host: &dyn crate::codec::cells::CodecHost) -> Option<String> {
    let mut token = [0u8; 24];
    if !crate::codec::dialect::fill_base62(host, &mut token, crate::codec::dialect::BASE62_ALPHABET)
    {
        return None;
    }

    let token = std::str::from_utf8(&token).unwrap_or("000000000000000000000000");
    Some(format!("req_01{token}"))
}
