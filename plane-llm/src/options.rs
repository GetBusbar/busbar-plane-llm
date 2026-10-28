//! The provider options: every value a dialect's far end takes in a
//! request head field or a query parameter that an operator may set once, statically, for a
//! provider.
//!
//! Each dialect declares its list ([`catalogue`]). The operator writes an option under the provider's
//! own name for it, directly in the provider's block, beside busbar's own provider keys:
//!
//! ```yaml
//! providers:
//!   openai:
//!     protocol: openai
//!     OpenAI-Organization: org-abc
//!     OpenAI-Project: proj_1
//!   vertex:
//!     protocol: gemini
//!     x-goog-user-project: my-gcp-project
//!   azure:
//!     protocol: openai
//!     api-version: 2024-10-21
//! ```
//!
//! THE RULE AT LOAD ([`read`]). Every key of a provider's block is either a busbar provider key
//! ([`BUSBAR_KEYS`]) or a name on the list of the dialect the provider speaks. Anything else is
//! refused, so the plane's `validate` and `open` both fail. The refusal names the key and lists
//! the names that dialect accepts. Head-field names match without regard to case and are echoed
//! in the provider's documented spelling. Query names match exactly. A credential or auth field
//! is on no list, and naming one is refused with that reason, as is a hop-by-hop or
//! connection-level field. A value of the wrong type is refused, saying what the name takes.
//!
//! THE PRECEDENCE ([`Precedence`]). A static option is the operator's DEFAULT. A same-dialect caller
//! that sends the same head field or query parameter on its own request keeps its own value, and
//! the static one is not sent. The exception is an option the dialect marks [`Precedence::Forced`]:
//! the operator's tenant or billing selectors. busbar governs those, so a caller's value of one
//! never passes and the operator's goes out on every request. A translated route forwards no
//! caller field, so every set option goes out on it.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// Where an option goes on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A request head field.
    Header,
    /// A request URL query parameter.
    Query,
}

/// How an option's value is judged at load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// One token: a non-empty string of letters, digits, `-`, `_` and `.`.
    Token,
    /// A non-empty string of visible ASCII and spaces, with no leading or trailing space.
    Text,
    /// One of these words, exactly.
    OneOf(&'static [&'static str]),
    /// A whole number, written as a number.
    Seconds,
    /// One or more tokens: a list of them, or one string of them joined by commas. Sent
    /// comma-joined.
    TokenList,
}

/// Whose value wins when a same-dialect caller sends the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precedence {
    /// The caller's own value wins. The static option is the operator's default.
    CallerWins,
    /// The operator's value always goes out and a caller's never does. This applies to tenant and
    /// billing selectors, which the dialect governs (`crate::dialect::Dialect::governed_headers`).
    Forced,
}

/// One option a dialect's far end accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireOption {
    /// The name in the provider's documented spelling.
    pub name: &'static str,
    /// Where it goes.
    pub place: Place,
    /// How its value is judged.
    pub check: Check,
    /// Whose value wins.
    pub precedence: Precedence,
    /// Sent only to a model served over Bedrock's `InvokeModel` envelope
    /// (`models.<m>.protocol: bedrock-invoke`). Bedrock's Converse operation takes the same
    /// settings in its body, and an option never writes a body.
    pub invoke_only: bool,
}

const fn header(name: &'static str, check: Check) -> WireOption {
    WireOption {
        name,
        place: Place::Header,
        check,
        precedence: Precedence::CallerWins,
        invoke_only: false,
    }
}

const fn forced_header(name: &'static str, check: Check) -> WireOption {
    WireOption {
        precedence: Precedence::Forced,
        ..header(name, check)
    }
}

const fn invoke_header(name: &'static str, check: Check) -> WireOption {
    WireOption {
        invoke_only: true,
        ..header(name, check)
    }
}

const fn query(name: &'static str, check: Check) -> WireOption {
    WireOption {
        place: Place::Query,
        ..header(name, check)
    }
}

/// Anthropic Messages API, `anthropic-version: 2023-06-01`.
/// Docs: <https://docs.anthropic.com/en/api/versioning> (`anthropic-version`, its two versions),
/// <https://docs.anthropic.com/en/api/beta-headers> (`anthropic-beta`, comma-separated),
/// <https://docs.anthropic.com/en/api/messages> (the request headers). `x-api-key` and
/// `authorization` are credentials and on no list.
pub const ANTHROPIC: &[WireOption] = &[
    header(
        "anthropic-version",
        Check::OneOf(&["2023-06-01", "2023-01-01"]),
    ),
    header("anthropic-beta", Check::TokenList),
];

/// OpenAI Chat Completions and Responses, and Azure OpenAI, which serves both.
/// Docs: <https://platform.openai.com/docs/api-reference/authentication> (`OpenAI-Organization`,
/// `OpenAI-Project`), <https://learn.microsoft.com/azure/ai-foundry/openai/reference> (the
/// `api-version` query parameter of an Azure OpenAI deployment).
/// The two selectors are the operator's tenant, so they are forced. `authorization` and `api-key`
/// are credentials.
pub const OPENAI: &[WireOption] = &[
    forced_header("OpenAI-Organization", Check::Token),
    forced_header("OpenAI-Project", Check::Token),
    query("api-version", Check::Token),
];

/// Gemini API (`generativelanguage.googleapis.com` v1beta) and Vertex AI, through Google's API
/// system parameters.
/// Docs: <https://cloud.google.com/apis/docs/system-parameters> (`X-Goog-User-Project`,
/// `X-Goog-Quota-User`, `X-Goog-Request-Reason`, `X-Server-Timeout`).
/// `X-Goog-User-Project` names the project the operator is billed under, so it is forced. `key`,
/// `x-goog-api-key` and `authorization` are credentials. The response-shaping parameters (`alt`,
/// `fields`, `prettyPrint`, `callback`) are on no list, because busbar reads the answer in the shape
/// it asked for.
pub const GEMINI: &[WireOption] = &[
    forced_header("X-Goog-User-Project", Check::Token),
    header("X-Goog-Quota-User", Check::Text),
    header("X-Goog-Request-Reason", Check::Text),
    header("X-Server-Timeout", Check::Seconds),
];

/// Amazon Bedrock runtime. These are the `InvokeModel` / `InvokeModelWithResponseStream` request
/// headers, which reach a model served over the invoke envelope. The Converse API takes the same
/// settings in its body (`guardrailConfig`, `performanceConfig`, `serviceTier`).
/// Docs: <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModel.html>.
/// The SigV4 fields (`authorization`, `x-amz-date`, `x-amz-content-sha256`,
/// `x-amz-security-token`) are credentials.
pub const BEDROCK: &[WireOption] = &[
    invoke_header(
        "X-Amzn-Bedrock-Trace",
        Check::OneOf(&["ENABLED", "DISABLED", "ENABLED_FULL"]),
    ),
    invoke_header("X-Amzn-Bedrock-GuardrailIdentifier", Check::Text),
    invoke_header("X-Amzn-Bedrock-GuardrailVersion", Check::Token),
    invoke_header(
        "X-Amzn-Bedrock-PerformanceConfig-Latency",
        Check::OneOf(&["standard", "optimized"]),
    ),
    invoke_header(
        "X-Amzn-Bedrock-Service-Tier",
        Check::OneOf(&["priority", "default", "flex"]),
    ),
];

/// Cohere v2 Chat.
/// Docs: <https://docs.cohere.com/reference/chat> (`X-Client-Name`). `authorization` is the
/// credential.
pub const COHERE: &[WireOption] = &[header("X-Client-Name", Check::Text)];

/// The options the far end of `dialect` accepts; none for a dialect the plane does not speak.
#[must_use]
pub fn catalogue(dialect: &str) -> &'static [WireOption] {
    match dialect {
        "anthropic" => ANTHROPIC,
        "openai" | "responses" => OPENAI,
        "gemini" => GEMINI,
        "bedrock" => BEDROCK,
        "cohere" => COHERE,
        _ => &[],
    }
}

/// busbar's own provider keys: the kernel's (the destination, the credential and its auth binding,
/// the health probe, the metadata carve-outs) and this plane's (the dialect, the error map, the lane
/// capabilities). Anything else in a provider's block must be on its dialect's list.
pub const BUSBAR_KEYS: &[&str] = &[
    "api_key",
    "protocol",
    "base_url",
    "error_map",
    "path",
    "path_base",
    "token_url",
    "scope",
    "subject",
    "auth",
    "allow_metadata_hosts",
    "health",
    "max_output_key",
    "anthropic_adaptive_thinking",
    "native_structured_output",
    "model_capabilities",
];

/// Credential and auth fields, which no option sets: busbar presents the provider's `api_key`
/// under its `auth` style, and no static setting replaces it.
const CREDENTIALS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "key",
    "api_key",
    "access_token",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
    "cookie",
];

/// Hop-by-hop and connection-level fields (RFC 9110 §7.6.1), which the transport writes for each
/// connection and no option sets.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// One option a provider sets, as it goes on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetOption {
    /// The name as written on the wire: a head field lower-cased, as every head field busbar
    /// writes is, or a query parameter as the provider spells it.
    pub wire: String,
    /// Where it goes.
    pub place: Place,
    /// The value, as text.
    pub value: String,
    /// Whose value wins.
    pub precedence: Precedence,
    /// Sent only over the invoke envelope ([`WireOption::invoke_only`]).
    pub invoke_only: bool,
}

/// The entry on `list` that `key` names: a head field without regard to case, a query parameter
/// exactly.
fn find(list: &'static [WireOption], key: &str) -> Option<&'static WireOption> {
    list.iter().find(|o| match o.place {
        Place::Header => o.name.eq_ignore_ascii_case(key),
        Place::Query => o.name == key,
    })
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_text(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// What a value of JSON type it is, for a refusal (the value itself is never echoed).
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "nothing",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a map",
    }
}

/// `value` judged against `check`, as the text it goes out as.
fn judge(check: Check, value: &Value) -> Result<String, String> {
    match (check, value) {
        (Check::Token, Value::String(s)) if is_token(s) => Ok(s.clone()),
        (Check::Token, _) => Err(format!(
            "takes one token (letters, digits, `-`, `_`, `.`), got {}",
            describe(value)
        )),
        (Check::Text, Value::String(s)) if is_text(s) => Ok(s.clone()),
        (Check::Text, _) => Err(format!(
            "takes a non-empty string of printable ASCII, got {}",
            describe(value)
        )),
        (Check::OneOf(words), Value::String(s)) if words.contains(&s.as_str()) => Ok(s.clone()),
        (Check::OneOf(words), _) => Err(format!(
            "takes one of {}, got {}",
            quoted(words.iter().copied()),
            describe(value)
        )),
        (Check::Seconds, Value::Number(n)) if n.is_u64() => Ok(n.to_string()),
        (Check::Seconds, _) => Err(format!(
            "takes a whole number of seconds, got {}",
            describe(value)
        )),
        (Check::TokenList, Value::String(s)) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            if parts.iter().all(|p| is_token(p)) {
                Ok(parts.join(","))
            } else {
                Err(format!(
                    "takes a list of tokens or one comma-separated string of them, got {}",
                    describe(value)
                ))
            }
        }
        (Check::TokenList, Value::Array(items))
            if !items.is_empty() && items.iter().all(|i| i.as_str().is_some_and(is_token)) =>
        {
            Ok(items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","))
        }
        (Check::TokenList, _) => Err(format!(
            "takes a list of tokens or one comma-separated string of them, got {}",
            describe(value)
        )),
    }
}

/// A value's type for a refusal; a string that has the right type but the wrong form says so.
fn describe(value: &Value) -> String {
    match value {
        Value::String(_) => "a string not of that form".to_string(),
        other => kind(other).to_string(),
    }
}

fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, n) in names.enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "`{n}`");
    }
    out
}

/// The names the far end of `dialect` accepts, in its documented spelling, for a refusal.
#[must_use]
pub fn accepted(dialect: &str) -> String {
    let list = catalogue(dialect);
    if list.is_empty() {
        format!("the {dialect} dialect accepts no options")
    } else {
        format!(
            "the {dialect} dialect accepts {}",
            quoted(list.iter().map(|o| o.name))
        )
    }
}

/// READ ONE PROVIDER'S OPTIONS: every key of its block that is not one of [`BUSBAR_KEYS`],
/// judged against the list of `dialect`, the dialect the provider speaks.
///
/// # Errors
///
/// The refusal, naming the provider and the key: a credential or auth field, a hop-by-hop field, a
/// name not on the dialect's list (with the list), or a value of the wrong type.
pub fn read(
    provider: &str,
    dialect: &str,
    block: &Map<String, Value>,
) -> Result<Vec<SetOption>, String> {
    let list = catalogue(dialect);
    let mut set = Vec::new();
    for (key, value) in block {
        if BUSBAR_KEYS.contains(&key.as_str()) {
            continue;
        }
        if let Some(opt) = find(list, key) {
            let text = judge(opt.check, value)
                .map_err(|why| format!("providers.{provider}.{}: {why}", opt.name))?;
            let wire = match opt.place {
                Place::Header => opt.name.to_ascii_lowercase(),
                Place::Query => opt.name.to_string(),
            };
            if set.iter().any(|s: &SetOption| s.wire == wire) {
                return Err(format!(
                    "providers.{provider}: `{}` is set twice (head-field names match without regard to case)",
                    opt.name
                ));
            }
            set.push(SetOption {
                wire,
                place: opt.place,
                value: text,
                precedence: opt.precedence,
                invoke_only: opt.invoke_only,
            });
            continue;
        }
        let lower = key.to_ascii_lowercase();
        if CREDENTIALS.contains(&lower.as_str()) || lower.starts_with("x-amz-") {
            return Err(format!(
                "providers.{provider}: `{key}` is a credential or auth field and cannot be set \
                 statically; busbar presents the provider's `api_key` under its `auth` style"
            ));
        }
        if HOP_BY_HOP.contains(&lower.as_str()) {
            return Err(format!(
                "providers.{provider}: `{key}` is a hop-by-hop or connection-level field and cannot \
                 be set statically; the transport writes it for each connection"
            ));
        }
        return Err(format!(
            "providers.{provider}: unknown key `{key}`: it is not a busbar provider key and not an \
             option of the provider's dialect; {}",
            accepted(dialect)
        ));
    }
    Ok(set)
}
