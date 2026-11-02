//! The OpenAI Responses webhook receiver, sans I/O: the plane's inbound leg for a background

use serde::Deserialize;
use serde_json::Value;

/// The top-level section this plane owns for its webhook receivers (a Statement section that is

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

pub const MALFORMED: &str =
    "malformed OpenAI Responses webhook event: expected a JSON object with a \
     non-empty string `type` and a `data` object carrying a non-empty string `id`";

/// Read a delivery's body. `type` and `data.id` are required non-empty strings; the optional `id`

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

#[must_use]
pub fn ack(event: &Event) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "received": true,
        "response_id": event.response_id,
        "type": event.event_type,
    }))
    .unwrap_or_default()
}
