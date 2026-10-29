// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Synthesized native-format Anthropic ids (`msg_…`, `req_…`) drawn from the OS CSPRNG.

use super::*;

/// Mint a native-shaped `msg_01` id for the cross-protocol path, where the backend supplied none.
/// Matching the native alphabet, prefix and length, with CSPRNG entropy, removes the tell.
pub(super) fn synth_message_id(host: &dyn busbar_contract::codec::CodecHost) -> String {
    synth_id_with_prefix(host, "msg_")
}

/// Mint a native-shaped `req_01` id for the top level of a synthesized error envelope, which
/// carries a `request_id` on the native API.
pub(super) fn synth_request_id(host: &dyn busbar_contract::codec::CodecHost) -> String {
    synth_id_with_prefix(host, "req_")
}

/// The same `req_01<24 base62>` id built from entropy the caller supplies (a plane may read no
/// random source; it is handed one). The same bytes give the same id.
///
/// `entropy` goes through the same rejection sampling as the drawn form and is cycled when shorter
/// than the draw needs, so even one byte yields a well-formed id, at the entropy supplied.
#[must_use]
pub fn request_id_from_entropy(entropy: &[u8]) -> String {
    const BASE62_REJECT_FLOOR: u8 = crate::codec::dialect::BASE62_REJECT_THRESHOLD;
    let alphabet = ANTHROPIC_NATIVE_ALPHABET;
    let mut token = [b'0'; SYNTH_ID_TOKEN_LEN];
    if !entropy.is_empty() {
        let mut filled = 0usize;
        // Every pass fills a character or the input has no in-range byte, which the round counter
        // catches.
        let mut rounds = 0usize;
        while filled < SYNTH_ID_TOKEN_LEN && rounds < SYNTH_ID_TOKEN_LEN + 1 {
            let mut progressed = false;
            for &byte in entropy {
                if byte >= BASE62_REJECT_FLOOR {
                    continue; // biased residue — discard, exactly as the drawn form does
                }
                token[filled] = alphabet[(byte % 62) as usize];
                filled += 1;
                progressed = true;
                if filled == SYNTH_ID_TOKEN_LEN {
                    break;
                }
            }
            if !progressed {
                break; // every supplied byte was out of range; the '0' fill stands
            }
            rounds += 1;
        }
    }
    // `token` is ASCII base62 by construction, hence always valid UTF-8.
    let token = std::str::from_utf8(&token).unwrap_or("000000000000000000000000");
    format!("req_01{token}")
}

/// Shared construction for `msg_` and `req_`: the `01` marker plus a 24-char token drawn entirely
/// from the OS CSPRNG, with no counter or clock (those leave predictable characters, a structural
/// tell). Never panics on the request path.
pub(super) fn synth_id_with_prefix(
    host: &dyn busbar_contract::codec::CodecHost,
    prefix: &str,
) -> String {
    // On an entropy failure the remaining '0' fill stands rather than panicking.
    let mut token = [b'0'; SYNTH_ID_TOKEN_LEN];
    crate::codec::dialect::fill_base62(host, &mut token, ANTHROPIC_NATIVE_ALPHABET);

    // `token` is ASCII base62, always valid UTF-8; the fallback keeps the path panic-free.
    let token = std::str::from_utf8(&token).unwrap_or("000000000000000000000000");
    format!("{prefix}01{token}")
}

/// Mint a request id (`req_01` + 24 base62 chars, 30 in all, matching the body `request_id`) for
/// the `request-id` response header a native response always carries; omitting it is a proxy tell.
/// Used on anthropic-ingress 2xx responses with no upstream `request-id` to forward. `None` (the
/// caller omits the header) only if entropy is unavailable. Uses the shared lowercase-first
/// `BASE62_ALPHABET`; a uniform draw over a permuted alphabet is still uniform.
pub fn synth_anthropic_request_id(host: &dyn busbar_contract::codec::CodecHost) -> Option<String> {
    // The `Option` contract (omit on entropy failure) differs from `synth_id_with_prefix`'s
    // '0'-fill, so this stays a separate call.
    let mut token = [0u8; 24];
    if !crate::codec::dialect::fill_base62(host, &mut token, crate::codec::dialect::BASE62_ALPHABET)
    {
        return None;
    }
    // token is ASCII base62, always valid UTF-8.
    let token = std::str::from_utf8(&token).unwrap_or("000000000000000000000000");
    Some(format!("req_01{token}"))
}
