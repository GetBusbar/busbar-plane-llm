// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The per-dialect all-in-one classifiers behind `ProtocolReader::classify` (test-only: the release
//! path runs `extract_error` + the host's `normalize_raw_error`). Each reader's `#[cfg(test)]
//! classify` delegates here, so the classification unit tests keep asserting exactly what they did.

use crate::codec::keys;
use busbar_contract::http::StatusCode;
use busbar_contract::upstream::{CanonicalSignal, StatusClass};

/// The anthropic reader's classifier.
pub(crate) fn anthropic(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);

    // context-length-exceeded (Anthropic returns 400 invalid_request_error). The lane
    // is healthy; this must fail over (to a larger-context model), not penalize the breaker.
    // Check before the generic 400/client-error path so it wins.
    let lower = text.to_lowercase();
    if lower.contains("prompt is too long")
        || (lower.contains("exceeds the maximum")
            && (lower.contains(keys::TOKEN) || lower.contains(keys::CONTEXT)))
    {
        return CanonicalSignal {
            class: StatusClass::ContextLength,
            provider_signal: Some("context_length".to_string()),
            retry_after: None,
        };
    }

    // Prefer the HTTP status, then structured error codes, then substrings as a fallback.
    // Parse the JSON once and examine `error.code` and `error.message` INDEPENDENTLY: the
    // message-substring billing/auth checks must fire even when the structured `code` field is
    // absent (some Anthropic error shapes carry a 200/non-401-403 body with only a message), so
    // they live OUTSIDE the `if let Some(code_val)` guard rather than nested inside it.
    if let Ok(json) = crate::codec::json::parse::<serde_json::Value>(body) {
        let error = json.get(keys::ERROR_WORD);

        if let Some(code_val) = error.and_then(|e| e.get(keys::CODE)) {
            if code_val.as_str() == Some("400") || code_val.as_str() == Some("422") {
                return CanonicalSignal {
                    class: StatusClass::ClientError,
                    provider_signal: Some("client_error".to_string()),
                    retry_after: None,
                };
            }
        }

        // Message-substring billing/auth detection — independent of `error.code` presence.
        if let Some(msg_str) = error
            .and_then(|e| e.get(keys::MESSAGE))
            .and_then(|m| m.as_str())
        {
            if msg_str.contains("nsufficient balance") {
                return CanonicalSignal {
                    class: StatusClass::Billing,
                    provider_signal: Some("billing".to_string()),
                    retry_after: None,
                };
            }
            if msg_str.contains(keys::UNAUTHORIZED) || msg_str.contains("invalid token") {
                return CanonicalSignal {
                    class: StatusClass::Auth,
                    provider_signal: Some("auth".to_string()),
                    retry_after: None,
                };
            }
        }
    }

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return CanonicalSignal {
            class: StatusClass::Auth,
            provider_signal: None,
            retry_after: None,
        };
    }

    if status.as_u16() == 429 {
        // Reuse the single lower-cased copy computed at the top of `classify` rather than
        // allocating a second one — on a verbose 429 body this avoids a redundant heap copy.
        if lower.contains("quota") && lower.contains("exhausted") {
            return CanonicalSignal {
                class: StatusClass::Billing,
                provider_signal: Some("429-quota-exhausted".to_string()),
                retry_after: None,
            };
        }
        return CanonicalSignal {
            class: StatusClass::RateLimit,
            provider_signal: Some("429-slowdown".to_string()),
            retry_after: None,
        };
    }

    if status.as_u16() >= 500 {
        return CanonicalSignal {
            class: StatusClass::ServerError,
            provider_signal: Some("5xx".to_string()),
            retry_after: None,
        };
    }

    if status.is_client_error() {
        return CanonicalSignal {
            class: StatusClass::ClientError,
            provider_signal: None,
            retry_after: None,
        };
    }

    CanonicalSignal {
        class: StatusClass::ClientError,
        provider_signal: None,
        retry_after: None,
    }
}

/// The cohere reader's classifier.
pub(crate) fn cohere(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();

    // Mirror the production `extract_error` gate: rate-limit / auth / server statuses are
    // classified by status FIRST, so a free-text body that mentions token counts on a 429/401/403
    // can never be reclassified as the no-penalty ContextLength class. The context-length phrasing
    // is honored ONLY on a request-size status (400 / 413).
    if status == StatusCode::TOO_MANY_REQUESTS {
        return CanonicalSignal {
            class: StatusClass::RateLimit,
            provider_signal: Some("429".to_string()),
            retry_after: None,
        };
    }

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return CanonicalSignal {
            class: StatusClass::Auth,
            provider_signal: Some("auth".to_string()),
            retry_after: None,
        };
    }

    if status.is_server_error() {
        return CanonicalSignal {
            class: StatusClass::ServerError,
            provider_signal: Some("5xx".to_string()),
            retry_after: None,
        };
    }

    if (status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE)
        && (lower.contains("too many tokens")
            || (lower.contains(keys::MAXIMUM) && lower.contains(keys::TOKENS)))
    {
        return CanonicalSignal {
            class: StatusClass::ContextLength,
            provider_signal: Some(
                crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string(),
            ),
            retry_after: None,
        };
    }

    if status.is_client_error() {
        return CanonicalSignal {
            class: StatusClass::ClientError,
            provider_signal: Some(format!("{}", status.as_u16())),
            retry_after: None,
        };
    }

    CanonicalSignal {
        class: StatusClass::ClientError,
        provider_signal: None,
        retry_after: None,
    }
}

/// The gemini reader's classifier.
pub(crate) fn gemini(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();

    // context-length-exceeded via message pattern
    if lower.contains("input is longer than the maximum number of tokens")
        || (lower.contains(keys::MAXIMUM_TOKENS) && lower.contains(keys::REQUESTED))
    {
        return CanonicalSignal {
            class: StatusClass::ContextLength,
            provider_signal: Some(
                crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string(),
            ),
            retry_after: None,
        };
    }

    // 429 → RateLimit
    if status == StatusCode::TOO_MANY_REQUESTS {
        return CanonicalSignal {
            class: StatusClass::RateLimit,
            provider_signal: Some("429".to_string()),
            retry_after: None,
        };
    }

    // 401/403 → Auth
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return CanonicalSignal {
            class: StatusClass::Auth,
            provider_signal: Some(keys::AUTH_WORD.to_string()),
            retry_after: None,
        };
    }

    // 5xx → ServerError
    if status.is_server_error() {
        return CanonicalSignal {
            class: StatusClass::ServerError,
            provider_signal: Some("5xx".to_string()),
            retry_after: None,
        };
    }

    // 4xx (other) → ClientError
    if status.is_client_error() {
        return CanonicalSignal {
            class: StatusClass::ClientError,
            provider_signal: Some(format!("{}", status.as_u16())),
            retry_after: None,
        };
    }

    CanonicalSignal {
        class: StatusClass::ClientError,
        provider_signal: None,
        retry_after: None,
    }
}

/// The bedrock reader's classifier.
pub(crate) fn bedrock(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();

    // Keep this set of context-length phrasings in LOCKSTEP with the production
    // `extract_error` above (the third `exceeds the maximum` pattern was once added there but
    // not here, drifting the two). All three must match identically so the test-only classifier
    // mirrors what the breaker actually sees. The `status == 400` gate is ALSO part of that
    // lockstep: `extract_error` only runs the body-scan override on a 400
    // ValidationException, so a 5xx body that happens to echo context-length phrasing must NOT
    // be reclassified as ContextLength here either — it falls through to the ServerError arm
    // below.
    if status == StatusCode::BAD_REQUEST
        && (lower.contains("input is longer than the maximum number of tokens")
            || (lower.contains(keys::MAXIMUM_TOKENS) && lower.contains(keys::REQUESTED))
            || (lower.contains("exceeds the maximum")
                && (lower.contains(keys::TOKEN) || lower.contains(keys::CONTEXT))))
    {
        return CanonicalSignal {
            class: StatusClass::ContextLength,
            provider_signal: Some(
                crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string(),
            ),
            retry_after: None,
        };
    }

    if status == StatusCode::TOO_MANY_REQUESTS {
        return CanonicalSignal {
            class: StatusClass::RateLimit,
            provider_signal: Some("429".to_string()),
            retry_after: None,
        };
    }

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return CanonicalSignal {
            class: StatusClass::Auth,
            provider_signal: Some(keys::AUTH_WORD.to_string()),
            retry_after: None,
        };
    }

    if status.is_server_error() {
        return CanonicalSignal {
            class: StatusClass::ServerError,
            provider_signal: Some("5xx".to_string()),
            retry_after: None,
        };
    }

    if status.is_client_error() {
        return CanonicalSignal {
            class: StatusClass::ClientError,
            provider_signal: Some(format!("{}", status.as_u16())),
            retry_after: None,
        };
    }

    CanonicalSignal {
        class: StatusClass::ClientError,
        provider_signal: None,
        retry_after: None,
    }
}

/// Canonical bearer-envelope error classification, shared verbatim by `OpenAiReader::classify` and
/// `ResponsesReader::classify` (the two were word-for-word identical). Both surfaces emit the same
/// OpenAI error envelope, so the mapping — context-length-exceeded (fail over without penalty) first,
/// then 429→RateLimit, 401/403→Auth, 5xx→ServerError, other 4xx→ClientError — is single-sourced here,
/// beside `dialect::bearer_error_code`, so neither dialect imports the other (design F3 SELF-CONTAINED).
///
/// `status` is the HTTP status code. It is test-only (the production
/// classification path is `OpenAiReader::extract_error` / `ResponsesReader::extract_error`, which this
/// mirrors for the confinement/parity test suite), so it stays `#[cfg(test)]` here exactly as it was
/// `#[cfg(any(test, feature = "test-support"))]` in the substrate — no other crate named it.
pub(crate) fn bearer(status: u16, body: &[u8]) -> CanonicalSignal {
    // context-length-exceeded — the lane is healthy; this must fail over (to a larger-context
    // model), not penalize the breaker. Detect by OpenAI code/message first.
    let code_is_context = crate::codec::json::parse::<serde_json::Value>(body)
        .ok()
        .and_then(|j| {
            j.get(keys::ERROR_WORD)
                .and_then(|e| e.get(keys::CODE))
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
        })
        .as_deref()
        == Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH);
    // Mirror production `extract_error`: the prose message scan is GATED to the HTTP statuses an
    // oversized request actually uses (400 invalid_request_error; 413 payload-too-large). Without the
    // gate a 401/429/5xx whose prose happens to contain "maximum context length" would reclassify as
    // ContextLength — letting a genuine auth/rate-limit/server failure escape fault attribution. The
    // structured `code: "context_length_exceeded"` path is NOT gated (it is unambiguous).
    //
    // The scan itself is the shared one and not a clause of its own: production runs all four
    // phrasings through `context_length_prose_scan`, and a copy here that carried only the
    // first was a mirror that showed a different picture. Every test proving oversized-request
    // failover through this function was then proving behaviour production does not have, for three
    // of the four phrasings the providers actually send.
    let oversized = status == 400 || status == 413;
    let prose_is_context = oversized
        && crate::codec::dialect::context_length_prose_scan(
            &String::from_utf8_lossy(body).to_lowercase(),
        );
    if code_is_context || prose_is_context {
        return CanonicalSignal {
            class: StatusClass::ContextLength,
            provider_signal: Some(
                crate::codec::dialect::PROVIDER_SIGNAL_CONTEXT_LENGTH.to_string(),
            ),
            retry_after: None,
        };
    }

    if status == 429 {
        return CanonicalSignal {
            class: StatusClass::RateLimit,
            provider_signal: Some("429".to_string()),
            retry_after: None,
        };
    }

    if status == 401 || status == 403 {
        return CanonicalSignal {
            class: StatusClass::Auth,
            provider_signal: Some(keys::AUTH_WORD.to_string()),
            retry_after: None,
        };
    }

    if (500..600).contains(&status) {
        return CanonicalSignal {
            class: StatusClass::ServerError,
            provider_signal: Some("5xx".to_string()),
            retry_after: None,
        };
    }

    if (400..500).contains(&status) {
        return CanonicalSignal {
            class: StatusClass::ClientError,
            provider_signal: Some(format!("{status}")),
            retry_after: None,
        };
    }

    CanonicalSignal {
        class: StatusClass::ClientError,
        provider_signal: None,
        retry_after: None,
    }
}
