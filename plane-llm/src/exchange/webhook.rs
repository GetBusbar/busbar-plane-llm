//! The OpenAI Responses webhook receiver, sans I/O: the plane's inbound leg for a background
//! Responses turn's completion webhook.
//!
//! A client creating a Responses turn with `background: true` gets a `resp_…` id at once; when the
//! turn finishes, the provider delivers a signed webhook to a URL the operator registered. This module
//! reads that delivery's body and answers its acknowledgement.
//!
//! Who decides what:
//! - On or off is the plugin's own setting, never a kernel or core switch: the plane owns the
//!   top-level `webhooks:` section ([`SECTION`]) and claims [`PATH`] only when it says
//!   `openai_responses: true`. Without the key the path answers as any unclaimed path.
//! - The signature is the auth kind's: the operator's `auth.chain` names a webhook-signature auth
//!   step (the Standard Webhooks scheme), which verifies the HMAC over the body and refuses a replayed
//!   `webhook-id` through the claim record. A request with none of its headers is all-PASS, so 401 end
//!   to end. This plane never sees the signing secret, and its unit requires a principal.
//! - The unit is the plane's own to answer (`ROUTE_LOCAL`): no walk, no far end, nothing billed.
//!
//! Busbar is a translator, not a conversation-state store: the receiver authenticates (through the
//! auth kind) and acknowledges the event, echoing its correlation id. It does not resume the turn.
use serde::Deserialize;
use serde_json::Value;

/// The top-level section this plane owns for its webhook receivers (a Statement section that is
/// neither declaring nor consumed: the plane opens with it as written).
pub const SECTION: &str = "webhooks";

/// The path the OpenAI Responses receiver answers on: exact, POST only.
pub const PATH: &str = "/v1/llm/webhooks/openai";

/// The section's grammar: which receivers this deployment serves. Every receiver is off unless
/// named `true`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhooks {
    /// Serve the OpenAI Responses background-turn webhook at [`PATH`].
    #[serde(default)]
    pub openai_responses: bool,
}

/// The receivers the plane's owned sections switch on (`PlaneOpenIn::owned`: one JSON object keyed by
/// section name, empty when the document writes none of them).
///
/// # Errors
///
/// The section's own words for a `webhooks:` block that does not read (an unknown key, or a value of
/// the wrong type). A boot refusal names it; a receiver silently left off would not.
pub fn from_owned(owned: &[u8]) -> Result<Webhooks, String> {
    if owned.is_empty() {
        return Ok(Webhooks::default());
    }
    let all: Value = serde_json::from_slice(owned).map_err(|e| format!("`{SECTION}`: {e}"))?;
    match all.get(SECTION) {
        None | Some(Value::Null) => Ok(Webhooks::default()),
        Some(section) => Webhooks::deserialize(section).map_err(|e| format!("`{SECTION}`: {e}")),
    }
}

/// Whether `(method, path)` is the Responses receiver's arrival.
#[must_use]
pub fn is_receiver(method: &str, path: &str) -> bool {
    method == "POST" && path == PATH
}

/// A verified delivery, read: the fields the acknowledgement echoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The event kind verbatim (`response.completed`, `response.failed`, …).
    pub event_type: String,
    /// The `resp_…` correlation id at `data.id`.
    pub response_id: String,
}

/// Why a delivery's body was refused (400): it is not a JSON object, or a field the receiver reads
/// is absent or wrong-typed. Unknown fields are tolerated.
pub const MALFORMED: &str =
    "malformed OpenAI Responses webhook event: expected a JSON object with a \
     non-empty string `type` and a `data` object carrying a non-empty string `id`";

/// Read a delivery's body. `type` and `data.id` are required non-empty strings; the optional `id`
/// (event id) and `created_at` are refused only when present and wrong-typed, as the ported receiver did.
/// # Errors
///
/// [`MALFORMED`], for any body that does not read.
pub fn read_event(body: &[u8]) -> Result<Event, &'static str> {
    let value: Value = serde_json::from_slice(body).map_err(|_| MALFORMED)?;
    let obj = value.as_object().ok_or(MALFORMED)?;
    let event_type = match obj.get("type") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err(MALFORMED),
    };
    let response_id = match obj
        .get("data")
        .and_then(Value::as_object)
        .map(|d| d.get("id"))
    {
        Some(Some(Value::String(s))) if !s.is_empty() => s.clone(),
        _ => return Err(MALFORMED),
    };
    match obj.get("id") {
        None | Some(Value::Null | Value::String(_)) => {}
        Some(_) => return Err(MALFORMED),
    }
    match obj.get("created_at") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_u64().is_some() => {}
        Some(_) => return Err(MALFORMED),
    }
    Ok(Event {
        event_type,
        response_id,
    })
}

/// THE ACKNOWLEDGEMENT a read delivery is answered with (200, `application/json`): receipt, and
/// the correlation id and kind echoed back, as the receiver this ports answered.
#[must_use]
pub fn ack(event: &Event) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "received": true,
        "response_id": event.response_id,
        "type": event.event_type,
    }))
    .unwrap_or_default()
}
