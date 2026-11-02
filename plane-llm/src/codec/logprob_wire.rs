// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The per-token logprob wire object two dialects speak alike: `{content: [{token, logprob, bytes,

use crate::codec::ir::{IrTokenLogprob, IrTopLogprob};
use crate::codec::keys;

fn malformed(field: &'static str) {
    crate::codec::drops::writer_drop!(
        crate::codec::drops::LOGPROBS,
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        [field,],
        "dropping malformed logprob entry: {field} cannot be represented without changing its data"
    );
}

fn read_bytes(entry: &serde_json::Value) -> Result<(Option<Vec<u8>>, bool), ()> {
    match entry.get(keys::BYTES) {
        None => Ok((None, false)),
        Some(serde_json::Value::Null) => Ok((None, true)),
        Some(value) => value
            .as_array()
            .ok_or(())?
            .iter()
            .map(|byte| {
                byte.as_u64()
                    .and_then(|byte| u8::try_from(byte).ok())
                    .ok_or(())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|bytes| (Some(bytes), false)),
    }
}

fn read_entry(entry: &serde_json::Value) -> Option<IrTopLogprob> {
    let token = entry.get(keys::TOKEN).and_then(|value| value.as_str());
    let logprob = entry.get(keys::LOGPROB).and_then(|value| value.as_f64());
    match (token, logprob, read_bytes(entry)) {
        (Some(token), Some(logprob), Ok((bytes, bytes_null))) => Some(IrTopLogprob {
            token: token.to_string(),
            logprob,
            bytes,
            bytes_null,
        }),
        _ => {
            malformed("token/logprob/bytes");
            None
        }
    }
}

/// OpenAI's `logprobs` object to neutral IR entries. Byte vectors are all-or-nothing: filtering an
pub fn read_token_logprobs(v: Option<&serde_json::Value>) -> Vec<IrTokenLogprob> {
    let entries = match v.and_then(|logprobs| logprobs.get(keys::CONTENT)) {
        None | Some(serde_json::Value::Null) => return Vec::new(),
        Some(content) => match content.as_array() {
            Some(entries) => entries,
            None => {
                malformed("content");
                return Vec::new();
            }
        },
    };
    entries
        .iter()
        .filter_map(|entry| {
            let chosen = read_entry(entry)?;
            let top = match entry.get(keys::TOP_LOGPROBS) {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(top) => match top.as_array() {
                    Some(top) => top.iter().filter_map(read_entry).collect(),
                    None => {
                        malformed("top_logprobs");
                        Vec::new()
                    }
                },
            };
            Some(IrTokenLogprob {
                token: chosen.token,
                logprob: chosen.logprob,
                bytes: chosen.bytes,
                bytes_null: chosen.bytes_null,
                top,
            })
        })
        .collect()
}

/// Neutral IR logprobs to OpenAI's `logprobs` object. `bytes` is synthesized from the token's UTF-8
pub fn write_token_logprobs(lps: &[IrTokenLogprob]) -> serde_json::Value {
    let content: Vec<serde_json::Value> = lps
        .iter()
        .map(|lp| {
            let bytes = write_bytes(&lp.token, lp.bytes.as_deref(), lp.bytes_null);
            let top: Vec<serde_json::Value> = lp
                .top
                .iter()
                .map(|t| {
                    let b = write_bytes(&t.token, t.bytes.as_deref(), t.bytes_null);
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

    serde_json::json!({ (keys::CONTENT): content, (keys::REFUSAL): serde_json::Value::Null })
}

fn write_bytes(token: &str, bytes: Option<&[u8]>, bytes_null: bool) -> serde_json::Value {
    match bytes {
        Some(bytes) => serde_json::json!(bytes),
        None if bytes_null => serde_json::Value::Null,
        None => serde_json::json!(token.as_bytes()),
    }
}
