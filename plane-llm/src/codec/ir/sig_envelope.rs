// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR-18 signature-provenance envelope (architect ruling).

use super::IrSignatureOrigin;

/// The envelope's version prefix. A vendor-minted signature never starts with it.
pub const ENVELOPE_PREFIX: &str = "bbsig1:";

impl IrSignatureOrigin {
    /// The envelope's origin word.
    pub fn as_envelope_word(self) -> &'static str {
        match self {
            IrSignatureOrigin::Anthropic => "anthropic",
            IrSignatureOrigin::Gemini => "gemini",
            IrSignatureOrigin::OpenAi => "openai",
            IrSignatureOrigin::BedrockOther => "bedrock",
        }
    }

    /// Inverse of [`Self::as_envelope_word`]; an unknown word is `None`.
    pub fn from_envelope_word(word: &str) -> Option<Self> {
        match word {
            "anthropic" => Some(IrSignatureOrigin::Anthropic),
            "gemini" => Some(IrSignatureOrigin::Gemini),
            "openai" => Some(IrSignatureOrigin::OpenAi),
            "bedrock" => Some(IrSignatureOrigin::BedrockOther),
            _ => None,
        }
    }
}

/// Wrap `signature` (minted by `origin`) for a carrier with no provenance field. Idempotent: an

pub fn wrap(origin: IrSignatureOrigin, signature: &str) -> String {
    if unwrap(signature).is_some() {
        return signature.to_string();
    }
    format!(
        "{ENVELOPE_PREFIX}{}:{}",
        origin.as_envelope_word(),
        b64url_encode(signature.as_bytes())
    )
}

/// Unwrap an envelope: `Some((origin, original signature))`, or `None` when `carried` is not a

pub fn unwrap(carried: &str) -> Option<(IrSignatureOrigin, String)> {
    let rest = carried.strip_prefix(ENVELOPE_PREFIX)?;
    let (word, payload) = rest.split_once(':')?;
    let origin = IrSignatureOrigin::from_envelope_word(word)?;
    let bytes = b64url_decode(payload)?;
    String::from_utf8(bytes).ok().map(|s| (origin, s))
}

/// READER helper: a signature read off a carrier whose own family is `own`. An envelope yields the

pub fn read_carried(
    carried: String,
    own: Option<IrSignatureOrigin>,
) -> (String, Option<IrSignatureOrigin>) {
    match unwrap(&carried) {
        Some((origin, sig)) => (sig, Some(origin)),
        None => (carried, own),
    }
}

/// [`read_carried`] for an optional carrier: `(None, None)` when no signature was carried.
pub fn read_carried_opt(
    carried: Option<String>,
    own: Option<IrSignatureOrigin>,
) -> (Option<String>, Option<IrSignatureOrigin>) {
    match carried {
        Some(c) => {
            let (sig, origin) = read_carried(c, own);
            (Some(sig), origin)
        }
        None => (None, None),
    }
}

/// Client-writer helper: the string for a client carrier for a signature minted by `origin`, where

pub fn for_client(
    signature: &str,
    origin: Option<IrSignatureOrigin>,
    accepts: impl Fn(IrSignatureOrigin) -> bool,
) -> String {
    match origin {
        Some(o) if !accepts(o) => wrap(o, signature),
        _ => signature.to_string(),
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 §5 base64url, no padding.
fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let emit = chunk.len() + 1;
        for i in 0..emit {
            let idx = ((n >> (18 - 6 * i)) & 0x3f) as usize;
            out.push(char::from(B64URL[idx]));
        }
    }
    out
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        B64URL.iter().position(|&x| x == c).map(|p| p as u32)
    }
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        let produced = chunk.len() - 1;
        for i in 0..produced {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }

        let used_bits = 6 * chunk.len();
        let kept_bits = 8 * produced;
        if used_bits > kept_bits {
            let mask = (1u32 << (used_bits - kept_bits)) - 1;
            if (n >> (24 - used_bits)) & mask != 0 {
                return None;
            }
        }
    }
    Some(out)
}
