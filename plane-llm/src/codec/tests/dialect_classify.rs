// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The per-dialect all-in-one classifiers behind `ProtocolReader::classify` (test-only: the release

use crate::codec::keys;
use busbar_contract::http::StatusCode;
use busbar_contract::upstream::{CanonicalSignal, StatusClass};

/// The anthropic reader's classifier.
pub(crate) fn anthropic(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);

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

/// The bedrock reader's classifier.
pub(crate) fn bedrock(status: StatusCode, body: &[u8]) -> CanonicalSignal {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();

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
pub(crate) fn bearer(status: u16, body: &[u8]) -> CanonicalSignal {
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
