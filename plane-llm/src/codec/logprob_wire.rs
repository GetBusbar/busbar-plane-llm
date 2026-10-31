// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The per-token logprob wire object two dialects speak alike: `{content: [{token, logprob, bytes,
//! top_logprobs[]}], refusal}` on a Chat choice, whose `content` entries are also the `LogProb`
//! entries a Responses `output_text` part carries. Both read and write it here rather than one
//! importing the other (design F3 self-contained).

use crate::codec::ir::{IrTokenLogprob, IrTopLogprob};
use crate::codec::keys;

/// OpenAI's `logprobs` object to neutral IR entries. `bytes` is preserved verbatim when present (a
/// token can be a partial UTF-8 fragment, so the byte array is the only faithful carrier).
pub fn read_token_logprobs(v: Option<&serde_json::Value>) -> Vec<IrTokenLogprob> {
    let entries = match v
        .and_then(|lp| lp.get(keys::CONTENT))
        .and_then(|c| c.as_array())
    {
        Some(a) => a,
        None => return Vec::new(),
    };
    let read_bytes = |e: &serde_json::Value| -> Option<Vec<u8>> {
        e.get(keys::BYTES)?.as_array().map(|arr| {
            arr.iter()
                .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                .collect()
        })
    };
    entries
        .iter()
        .filter_map(|e| {
            Some(IrTokenLogprob {
                token: e.get(keys::TOKEN)?.as_str()?.to_string(),
                logprob: e.get(keys::LOGPROB)?.as_f64()?,
                bytes: read_bytes(e),
                top: e
                    .get(keys::TOP_LOGPROBS)
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| {
                                Some(IrTopLogprob {
                                    token: t.get(keys::TOKEN)?.as_str()?.to_string(),
                                    logprob: t.get(keys::LOGPROB)?.as_f64()?,
                                    bytes: read_bytes(t),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Neutral IR logprobs to OpenAI's `logprobs` object. `bytes` is synthesized from the token's UTF-8
/// when the source protocol (Gemini) carries none, the value OpenAI itself returns for a whole-token string.
pub fn write_token_logprobs(lps: &[IrTokenLogprob]) -> serde_json::Value {
    let content: Vec<serde_json::Value> = lps
        .iter()
        .map(|lp| {
            let bytes = lp
                .bytes
                .clone()
                .unwrap_or_else(|| lp.token.as_bytes().to_vec());
            let top: Vec<serde_json::Value> = lp
                .top
                .iter()
                .map(|t| {
                    let b = t
                        .bytes
                        .clone()
                        .unwrap_or_else(|| t.token.as_bytes().to_vec());
                    serde_json::json!({(keys::TOKEN): t.token, (keys::LOGPROB): t.logprob, (keys::BYTES): b})
                })
                .collect();
            serde_json::json!({
                (keys::TOKEN): lp.token,
                (keys::LOGPROB): lp.logprob,
                (keys::BYTES): bytes,
                (keys::TOP_LOGPROBS): top
            })
        })
        .collect();
    // `refusal` is required (nullable) in the choice `logprobs` object in both published schemas
    // (`CreateChatCompletionResponse` and `...StreamResponse`: `required: ["content", "refusal"]`). The
    // IR carries no refusal tokens (a refusal arrives as text), so emit explicit null, as OpenAI does
    // for a non-refusing completion. Emitting only `content` failed strict spec validation and the
    // Python SDK's Pydantic model, a proxy tell on every logprobs response. The Responses writer lifts
    // the `content` array out for an `output_text` part's bare `LogProb[]`, so it is unaffected.
    serde_json::json!({ (keys::CONTENT): content, (keys::REFUSAL): serde_json::Value::Null })
}
