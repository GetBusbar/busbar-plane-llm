//! The six dialects' egress facts: the outbound auth style and its parameters, and the request
//! and response head fields and query parameters busbar governs for each.

/// One dialect's egress facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// The dialect's registry name, the same string the codec crate answers to.
    pub name: &'static str,
    /// The egress-auth scheme that decorates a request to an upstream of this dialect.
    pub egress_scheme: &'static str,
    /// THE DIALECT'S DEFAULT OUTBOUND AUTH STYLE (the design's auth points: a member's style is its
    /// provider's `auth:`, else its plane's default for its dialect): the
    /// style an auth plugin serves, stated in the plane tail's `dialect_auth`.
    pub egress_style: &'static str,
    /// THE DEFAULT STYLE'S PARAMETERS for this dialect, a JSON object (`""`: none), stated in the
    /// plane tail's `dialect_auth.params` and handed the auth plugin's `open_outbound` as its
    /// settings at seal, under the provider's own.
    pub egress_params: &'static str,
    /// The request headers busbar GOVERNS for this dialect (lower-case): its credential headers and
    /// its tenant selectors (the forced options of `crate::options::catalogue`). A same-dialect
    /// route forwards every other client header unchanged; these never pass, because busbar's
    /// upstream credential and configuration replace them.
    pub governed_headers: &'static [&'static str],
    /// The request URL query parameters busbar GOVERNS for this dialect: its credential parameters.
    /// A same-dialect route forwards every other caller parameter unchanged; these never pass.
    pub governed_query: &'static [&'static str],
    /// The response headers busbar GOVERNS for this dialect (lower-case): what the far end derives
    /// from busbar's own credential and tenant (the operator's organization or project id). A
    /// same-dialect answer relays every other upstream header; these never reach the caller.
    pub governed_response_headers: &'static [&'static str],
}

/// The upstream answer head fields that describe the OPERATOR's own upstream account or connection,
/// in every dialect: the cookies a far end (or its CDN) sets for busbar's connection. A tenant never
/// receives them; the previous release relayed only the request id. Exact names, so the host strips
/// them before the plane reads the head ([`crate::plane_door::DENY_RESPONSE_HEADERS`]).
pub const ACCOUNT_RESPONSE_HEADERS: &[&str] = &["set-cookie", "set-cookie2"];

/// The upstream answer head field families that report the operator's account-wide rate-limit state
/// (`x-ratelimit-remaining-tokens`, `anthropic-ratelimit-requests-limit`, ...), in every dialect:
/// one tenant must not read the operator's (or another tenant's) quota. Prefixes, lower-case.
pub const ACCOUNT_RESPONSE_PREFIXES: &[&str] = &["x-ratelimit-", "anthropic-ratelimit-"];

/// Whether the answer head field `name` (compared without case) is the operator's account state
/// ([`ACCOUNT_RESPONSE_HEADERS`], [`ACCOUNT_RESPONSE_PREFIXES`]), never relayed to a caller.
#[must_use]
pub fn account_scoped_response(name: &str) -> bool {
    ACCOUNT_RESPONSE_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
        || ACCOUNT_RESPONSE_PREFIXES.iter().any(|p| {
            name.len() >= p.len() && name.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
        })
}

/// What the two dialects of one vendor govern on an answer: the operator's tenant ids it echoes.
const OPENAI_GOVERNED_RESPONSE: &[&str] = &["openai-organization", "openai-project"];

/// What the two dialects of one vendor govern: its credential headers (a bearer, and the key header
/// a re-hosted deployment of it reads) and its two tenant selectors, which only the provider's
/// forced options (`crate::options::OPENAI`) set.
const OPENAI_GOVERNED: &[&str] = &[
    "authorization",
    "api-key",
    "openai-organization",
    "openai-project",
];

/// The `api-key` style's parameters for the dialect whose credentials come in families (its
/// codec's `EgressScheme::Static`): an api key presented in `x-api-key` with leading whitespace
/// trimmed, an oauth token as a bearer, the operator's own key in `x-api-key`, a passthrough caller's
/// as a bearer.
pub(crate) const KEY_FAMILY_PARAMS: &str = concat!(
    r#"{"header":"x-api-key","#,
    r#""families":[{"prefix":"sk-ant-api","header":"x-api-key","trim_start":true},"#,
    r#"{"prefix":"sk-ant-oat"}],"#,
    r#""own":{"header":"x-api-key"},"passthrough":{},"protocol":"anthropic"}"#,
);

/// The bearer-presented dialects' parameters (their codecs' `EgressScheme::Static` with no
/// credential families, the operator's and a passthrough caller's credential both as a bearer),
/// naming the dialect as the `protocol` a credential no header may carry is logged under (1.5.5's
/// `protocol=` field).
pub(crate) const OPENAI_PARAMS: &str =
    r#"{"families":[],"own":{},"passthrough":{},"protocol":"openai"}"#;
/// [`OPENAI_PARAMS`] for the responses dialect.
pub(crate) const RESPONSES_PARAMS: &str =
    r#"{"families":[],"own":{},"passthrough":{},"protocol":"responses"}"#;
/// [`OPENAI_PARAMS`] for the cohere dialect.
pub(crate) const COHERE_PARAMS: &str =
    r#"{"families":[],"own":{},"passthrough":{},"protocol":"cohere"}"#;
/// The `x-goog-api-key`-presented dialect's parameters (its codec's `EgressScheme::Static`): the
/// operator's and a passthrough caller's credential verbatim in `x-goog-api-key`, under its
/// `protocol`.
pub(crate) const GEMINI_PARAMS: &str = concat!(
    r#"{"families":[],"own":{"header":"x-goog-api-key"},"#,
    r#""passthrough":{"header":"x-goog-api-key"},"protocol":"gemini"}"#,
);

/// The `sigv4` style's parameters for the signed dialect (its codec's `EgressScheme::SigV4`): the
/// service, the content type the signature covers, and the region read from the provider's host by
/// the codec's rule (`derive_sigv4_region`), else its default with the codec's warning.
pub(crate) const SIGNED_PARAMS: &str = concat!(
    r#"{"service":"bedrock","content_type":"application/json","#,
    r#""region":{"host_label_after":["bedrock-runtime","bedrock-runtime-fips","bedrock","bedrock-fips"],"#,
    r#""default":"us-east-1","#,
    r#""unread":"could not derive AWS region from Bedrock endpoint host; defaulting SigV4 scope to "#,
    r#"us-east-1 (set a bedrock-runtime[-fips].<region>.amazonaws.com host)"}}"#,
);

/// The table, one row per dialect, in the order the codec crate declares them.
pub const DIALECTS: &[Dialect] = &[
    Dialect {
        name: "anthropic",
        egress_scheme: "bearer",
        egress_style: "api-key",
        egress_params: KEY_FAMILY_PARAMS,
        governed_headers: &["authorization", "x-api-key"],
        governed_query: &[],
        governed_response_headers: &["anthropic-organization-id"],
    },
    Dialect {
        name: "openai",
        // This dialect accepts a newer spelling as well, and both are declared. The reasoning
        // models of this vendor refuse the older key outright, so a client of one of them sends
        // only the newer; naming just the older was a hold sized off a key that never arrived.
        // This dialect reports no separate written-to-cache quantity.
        egress_scheme: "bearer",
        egress_style: "bearer",
        egress_params: OPENAI_PARAMS,
        governed_headers: OPENAI_GOVERNED,
        governed_query: &[],
        governed_response_headers: OPENAI_GOVERNED_RESPONSE,
    },
    Dialect {
        name: "gemini",
        // The model is in the request target, not the body.
        egress_scheme: "bearer",
        egress_style: "x-goog-api-key",
        egress_params: GEMINI_PARAMS,
        governed_headers: &["authorization", "x-goog-api-key", "x-goog-user-project"],
        governed_query: &["key"],
        governed_response_headers: &[],
    },
    Dialect {
        name: "bedrock",
        // The model is in the request target, not the body.
        egress_scheme: "request-signature",
        egress_style: "sigv4",
        egress_params: SIGNED_PARAMS,
        governed_headers: &[
            "authorization",
            "x-amz-date",
            "x-amz-content-sha256",
            "x-amz-security-token",
        ],
        governed_query: &[],
        governed_response_headers: &[],
    },
    Dialect {
        name: "responses",
        egress_scheme: "bearer",
        egress_style: "bearer",
        egress_params: RESPONSES_PARAMS,
        governed_headers: OPENAI_GOVERNED,
        governed_query: &[],
        governed_response_headers: OPENAI_GOVERNED_RESPONSE,
    },
    Dialect {
        name: "cohere",
        // This dialect reports no cache accounting at all.
        egress_scheme: "bearer",
        egress_style: "bearer",
        egress_params: COHERE_PARAMS,
        governed_headers: &["authorization"],
        governed_query: &[],
        governed_response_headers: &[],
    },
];

/// The row for one dialect, by name.
#[must_use]
pub fn dialect(name: &str) -> Option<&'static Dialect> {
    DIALECTS.iter().find(|d| d.name == name)
}
