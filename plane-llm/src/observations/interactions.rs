//! Gemini Interactions wire evidence; deliberately separate from generateContent usage.
//!
//! Field evidence: <https://ai.google.dev/api/interactions-api> and
//! <https://ai.google.dev/gemini-api/docs/background-execution>, checked 2026-11-10.
//! Thoughts are additional output. Tool-use prompts are an independent component whose rate
//! must be configured explicitly. `total_tokens` and modality breakdowns are diagnostic only.

use busbar_contract::abi::sdk::plane::observation::{Observation, ScopeId};
use busbar_contract::Count;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BODY: usize = 1024 * 1024;

/// A native Interactions operation. Retrieval and resumed streaming share an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// A new paid interaction, including a new continuation with a previous ID.
    Create,
    /// Retrieve a stored interaction, optionally resuming its event stream.
    Get {
        /// Provider ID.
        id: String,
        /// Stream selection.
        stream: bool,
        /// Opaque provider event cursor; never an accounting revision.
        last_event_id: Option<String>,
    },
    /// Request cancellation; authoritative incurred usage can still settle afterwards.
    Cancel {
        /// Provider ID.
        id: String,
    },
    /// Remove content while retaining accounting tombstones.
    Delete {
        /// Provider ID.
        id: String,
    },
}

/// A validated create envelope; the original body remains the bytes sent upstream.
#[derive(Debug, PartialEq, Eq)]
pub struct Create {
    /// Caller-selected route name, resolved and frozen before the scope is issued.
    pub model: String,
    /// Requested provider service tier, frozen into the admitted accounting binding.
    pub pricing_mode: String,
    /// Context from another run. This never reuses that run's accounting scope.
    pub previous_interaction_id: Option<String>,
    /// Background execution requested by the caller.
    pub background: bool,
    /// Event-stream response requested by the caller.
    pub stream: bool,
}

/// Validate the supported model-run variant before admission and any paid request.
/// Stored execution is required for request-driven reconciliation; agent variants and opaque
/// server continuation tokens need a separately approved identity/usage schema.
pub fn create(body: &[u8]) -> Result<Create, &'static str> {
    if body.len() > MAX_BODY {
        return Err("oversized create request");
    }
    let value: Value = crate::codec::json::parse(body).map_err(|_| "malformed create request")?;
    let object = value.as_object().ok_or("create request is not an object")?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or("create requires a model")?;
    if object.contains_key("agent") || object.contains_key("continuation_token") {
        return Err("unsupported agent or continuation-token variant");
    }
    let boolean = |key: &str, default| match object.get(key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err("invalid create boolean"),
    };
    if !boolean("store", true)? {
        return Err("observation accounting requires stored execution");
    }
    let previous_interaction_id = match object.get("previous_interaction_id") {
        None => None,
        Some(Value::String(id)) if valid_id(id) => Some(id.clone()),
        _ => return Err("invalid previous interaction ID"),
    };
    Ok(Create {
        model: model.into(),
        pricing_mode: pricing_mode(object.get("service_tier"))?.into(),
        previous_interaction_id,
        background: boolean("background", false)?,
        stream: boolean("stream", false)?,
    })
}

fn pricing_mode(value: Option<&Value>) -> Result<&str, &'static str> {
    match value {
        None => Ok("standard"),
        Some(Value::String(value))
            if matches!(value.as_str(), "standard" | "priority" | "flex") =>
        {
            Ok(value)
        }
        _ => Err("unsupported provider service tier"),
    }
}

/// Recognize the native versioned operation table without interpreting an ID as a route/model.
pub fn operation(method: &str, target: &str) -> Result<Operation, &'static str> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let rest = path
        .strip_prefix("/v1beta/interactions")
        .ok_or("not an Interactions path")?;
    if rest.is_empty() {
        return if method == "POST" && query.is_empty() {
            Ok(Operation::Create)
        } else {
            Err("unsupported create method or query")
        };
    }
    let rest = rest.strip_prefix('/').ok_or("invalid Interactions path")?;
    let (id, cancel) = rest
        .strip_suffix("/cancel")
        .map_or((rest, false), |id| (id, true));
    if !valid_id(id) {
        return Err("invalid interaction ID");
    }
    if cancel {
        return if method == "POST" && query.is_empty() {
            Ok(Operation::Cancel { id: id.into() })
        } else {
            Err("unsupported cancel method or query")
        };
    }
    match method {
        "DELETE" if query.is_empty() => Ok(Operation::Delete { id: id.into() }),
        "GET" => {
            let mut stream = None;
            let mut event = None;
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                let (key, value) = pair.split_once('=').ok_or("invalid query")?;
                match key {
                    "stream" if stream.is_none() => {
                        stream = Some(match value {
                            "true" => true,
                            "false" => false,
                            _ => return Err("invalid stream switch"),
                        })
                    }
                    "last_event_id" if event.is_none() && valid_id(value) => {
                        event = Some(value.into())
                    }
                    _ => return Err("unsupported or repeated query parameter"),
                }
            }
            if event.is_some() && stream != Some(true) {
                return Err("event cursor requires streaming");
            }
            Ok(Operation::Get {
                id: id.into(),
                stream: stream.unwrap_or(false),
                last_event_id: event,
            })
        }
        _ => Err("unsupported interaction operation"),
    }
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 192
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// Original configuration pinned before dispatch; supplied only by the admitted adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Host-issued original ownership and price-era scope.
    pub scope: ScopeId,
    /// Provider account namespace frozen at creation.
    pub account_namespace: String,
    /// Original concrete destination entry, recorded after route selection; polls never fail over.
    pub route_entry: String,
    /// Actual model selected at creation, never selected by a poll response.
    pub model: String,
    /// Original service tier; its host-issued scope freezes the associated rate era.
    pub pricing_mode: String,
    /// Operator-approved usage interpretation/version.
    pub usage_schema: String,
    /// Declared ordinary input class.
    pub input_class: u32,
    /// Declared cache-read class.
    pub cache_read_class: u32,
    /// Declared output class (thought component has the same output rate).
    pub output_class: u32,
    /// Explicitly priced tool-use prompt class; absent refuses nonzero tool usage.
    pub tool_use_class: Option<u32>,
    /// Classes confirmed priced by the admitted configuration, including explicit zero rates.
    pub priced_classes: Vec<u32>,
}

/// A protected provider response and its attributable original run.
pub struct Evidence<'a> {
    /// Original ID found through durable, principal-scoped lookup; `None` is historical/unknown.
    pub known_id: Option<&'a str>,
    /// Upstream response status, independent of interaction status.
    pub http_status: u16,
    /// Complete JSON object or one complete decoded SSE JSON event.
    pub body: &'a [u8],
    /// Protected raw-evidence reference; raw response is retained separately by its owner.
    pub evidence_ref: &'a [u8],
}

/// Normalization outcome, with unresolved evidence kept distinct from free usage.
#[derive(Debug, PartialEq, Eq)]
pub enum Normalized {
    /// Authoritative independent components; submit through the host, never `UnitCount` as well.
    Final {
        /// One observation per economic component.
        observations: Vec<Observation>,
        /// Wire-faithful usage, including optional modality arrays.
        usage: Value,
    },
    /// More evidence or an explicitly supported tariff is necessary.
    Pending(&'static str),
    /// No original durable ownership binding exists; do not invent a charging scope.
    Unattributed,
    /// A failed retrieval is not provider run usage.
    RetrievalFailed,
}

/// Normalize a final native snapshot without mutating the caller's response bytes.
pub fn normalize(binding: &Binding, evidence: Evidence<'_>) -> Normalized {
    let Some(known_id) = evidence.known_id else {
        return Normalized::Unattributed;
    };
    if !(200..300).contains(&evidence.http_status) {
        return Normalized::RetrievalFailed;
    }
    match normalize_inner(binding, known_id, evidence.body, evidence.evidence_ref) {
        Ok(value) => value,
        Err(reason) => Normalized::Pending(reason),
    }
}

fn normalize_inner(
    binding: &Binding,
    known_id: &str,
    bytes: &[u8],
    evidence: &[u8],
) -> Result<Normalized, &'static str> {
    if bytes.len() > MAX_BODY {
        return Err("oversized interaction evidence");
    }
    if binding.scope == [0; 32]
        || binding.account_namespace.is_empty()
        || binding.account_namespace.len() > 128
        || binding.model.is_empty()
        || evidence.is_empty()
        || evidence.len() > 1024
    {
        return Err("missing original binding or protected evidence");
    }
    if binding.usage_schema != "gemini-interactions-2026-11-10" {
        return Err("unsupported usage schema");
    }
    if !matches!(
        binding.pricing_mode.as_str(),
        "standard" | "priority" | "flex"
    ) {
        return Err("unsupported original pricing mode");
    }
    if binding.input_class == binding.cache_read_class
        || binding.input_class == binding.output_class
        || binding.cache_read_class == binding.output_class
    {
        return Err("overlapping declared token classes");
    }
    let root: Value =
        crate::codec::json::parse(bytes).map_err(|_| "malformed interaction evidence")?;
    let object = match root.get("event_type").and_then(Value::as_str) {
        Some("interaction.completed" | "interaction.status_update") => root
            .get("interaction")
            .ok_or("event lacks complete interaction")?,
        Some(_) => return Err("event is not authoritative final usage"),
        None => &root,
    };
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or("missing interaction ID")?;
    if id != known_id || !valid_id(id) {
        return Err("interaction identity mismatch");
    }
    if object
        .get("model")
        .is_some_and(|m| m.as_str() != Some(binding.model.as_str()))
        || object.get("agent").is_some()
    {
        return Err("unknown or changed model variant");
    }
    if object
        .get("service_tier")
        .is_some_and(|value| value.as_str() != Some(binding.pricing_mode.as_str()))
    {
        return Err("changed provider pricing mode");
    }
    match object.get("status").and_then(Value::as_str) {
        Some("completed" | "failed" | "cancelled" | "incomplete" | "budget_exceeded") => {}
        _ => return Err("run has no authoritative terminal snapshot"),
    }
    let usage = object
        .get("usage")
        .filter(|u| u.is_object())
        .ok_or("terminal run has no usage")?;
    let input = integer(usage, "total_input_tokens")?;
    let cache = integer(usage, "total_cached_tokens")?;
    let output = integer(usage, "total_output_tokens")?;
    let thoughts = integer(usage, "total_thought_tokens")?;
    let tools = integer(usage, "total_tool_use_tokens")?;
    let ordinary = input
        .checked_sub(cache)
        .map_err(|_| "invalid cached input partition")?;
    if ordinary.is_negative() {
        return Err("cached input exceeds total input");
    }
    // These are preserved as evidence, never added to their parent totals.
    for (field, total) in [
        ("input_tokens_by_modality", input),
        ("cached_tokens_by_modality", cache),
        ("output_tokens_by_modality", output),
        ("tool_use_tokens_by_modality", tools),
    ] {
        modality_breakdown(usage, field, total)?;
    }
    if let Some(total) = usage.get("total_tokens") {
        integer_value(total)?;
    }
    let key = serde_json::to_vec(&(binding.account_namespace.as_str(), id))
        .map_err(|_| "invalid economic key")?;
    if key.len() > 256 {
        return Err("economic key exceeds host bound");
    }
    let mut observations = Vec::new();
    for (component, class, amount) in [
        ("prompt", binding.input_class, ordinary),
        ("cache_read", binding.cache_read_class, cache),
        ("output", binding.output_class, output),
        ("thoughts", binding.output_class, thoughts),
    ] {
        observations.push(component_observation(
            binding, &key, component, class, amount, evidence,
        )?);
    }
    match binding.tool_use_class {
        Some(class) => observations.push(component_observation(
            binding,
            &key,
            "tool_use_prompt",
            class,
            tools,
            evidence,
        )?),
        None if tools != Count::ZERO => return Err("tool-use prompt tariff is unresolved"),
        None => {}
    }
    Ok(Normalized::Final {
        observations,
        usage: usage.clone(),
    })
}

fn component_observation(
    binding: &Binding,
    key: &[u8],
    component: &str,
    class: u32,
    count: Count,
    evidence: &[u8],
) -> Result<Observation, &'static str> {
    if !binding.priced_classes.contains(&class) {
        return Err("component class has no approved price");
    }
    Observation::new(binding.scope, key.to_vec(), component.as_bytes().to_vec())
        .count(class, count)
        .map(|o| o.final_totals().evidence(evidence.to_vec()))
        .map_err(|_| "invalid usage observation")
}

fn integer(usage: &Value, field: &str) -> Result<Count, &'static str> {
    integer_value(usage.get(field).ok_or("missing usage component")?)
}
fn integer_value(value: &Value) -> Result<Count, &'static str> {
    let count = crate::codec::usage_count::read_count(value).ok_or("invalid usage number")?;
    if !count.fits_a_64_bit_column() || count.micros() % 1_000_000 != 0 {
        return Err("usage number is fractional or excessive");
    }
    Ok(count)
}
fn modality_breakdown(usage: &Value, field: &str, total: Count) -> Result<(), &'static str> {
    let Some(value) = usage.get(field) else {
        return Ok(());
    };
    let rows = value
        .as_array()
        .filter(|r| r.len() <= 5)
        .ok_or("invalid modality breakdown")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut sum = Count::ZERO;
    for row in rows {
        let modality = row
            .get("modality")
            .and_then(Value::as_str)
            .ok_or("missing modality")?;
        if !matches!(modality, "text" | "image" | "audio" | "video" | "document")
            || !seen.insert(modality)
        {
            return Err("unknown or duplicate modality");
        }
        // This first adapter's aggregate rates are valid only for text. Preserve other evidence
        // for a modality-specific tariff rather than pricing it as text.
        let amount = integer(row, "tokens")?;
        if modality != "text" && amount != Count::ZERO {
            return Err("modality-specific pricing is unresolved");
        }
        sum = sum
            .checked_add(amount)
            .map_err(|_| "modality sum overflows")?;
    }
    if sum != total {
        return Err("modality breakdown contradicts aggregate");
    }
    Ok(())
}

#[cfg(test)]
#[path = "interactions_tests.rs"]
mod tests;
