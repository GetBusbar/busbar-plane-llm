// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Pure preparation for the bounded, buffered Gemini Interactions profile.
//!
//! This module does not dispatch requests or establish ownership. Before calling
//! it, the runtime must perform principal-scoped durable lookup for stored IDs and
//! continuations, obtain host custody for each new paid run, and pin the selected
//! destination. A continuation gets a new scope; its predecessor grants no scope
//! authority. Host admission must prove required classes have approved rates;
//! unexpected provider modalities/tool economics prevent final accounting closure.
//!
//! Supported native paths and text request fields were checked against
//! <https://aistudio.google.com/api-reference/interactions-api/create-interaction>
//! and <https://ai.google.dev/api/interactions-api> on 2026-11-10. Streaming,
//! hosted tools, agents, multimodal input/output controls and arbitrary query/path
//! overrides are deliberately refused by this first request profile.

use super::interactions::{self, Binding, Create, Operation};
use crate::exchange::attempt::{self, FarRequest};
use crate::exchange::shaping::{Lane, Shaping};
use crate::options::Place;
use busbar_contract::spans::{self, Edit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// Explicit per-model native opt-in; an empty configuration enables nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfig {
    /// Concrete shaping model entries, never pool names or arbitrary provider IDs.
    #[serde(default)]
    pub models: BTreeMap<String, NativeModel>,
}

/// Operator-approved account and service tiers for one ordinary text model.
///
/// This is not a rate card. Host admission must separately prove each configured
/// usage class has a rate; this structure never supplies a fallback tariff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModel {
    /// Stable provider account namespace used by durable ownership records.
    pub account_namespace: String,
    /// Explicitly enabled provider service tiers; currently only `standard` is supported.
    pub pricing_modes: BTreeSet<String>,
}

impl NativeConfig {
    /// Validate only opted-in lanes; unrelated models need no observation/store setup.
    pub fn validate(&self, shaping: &Shaping) -> Result<(), &'static str> {
        for (model, opt_in) in &self.models {
            let lane = shaping
                .lanes
                .get(model)
                .ok_or("unknown native model entry")?;
            if lane.model != *model {
                return Err("native model entry differs from selected lane");
            }
            validate_lane(lane, opt_in)?;
        }
        Ok(())
    }
}

/// A predecessor binding returned by the runtime's authenticated durable lookup.
/// Possessing these Rust fields is not an authorization check by itself.
pub struct Continuation<'a> {
    /// Exact owned predecessor ID; never inferred from a client-supplied binding.
    pub id: &'a str,
    /// Original selected destination/account/model; its scope is never reused.
    pub binding: &'a Binding,
}

/// Prepare one new paid create after route selection and predecessor ownership lookup.
///
/// The already parsed envelope must still agree with these exact body bytes. Only
/// the top-level model string is spliced; every other supported byte is retained.
/// Head fields contain safe native defaults only. The runtime must apply its shared
/// auth-governed caller-field forwarding helper before dispatch; credentials remain
/// entirely host-owned. This function never accepts a caller-provided target.
pub fn prepare_create(
    lane: &Lane,
    opt_in: &NativeModel,
    create: &Create,
    body: &[u8],
    continuation: Option<Continuation<'_>>,
) -> Result<FarRequest, &'static str> {
    validate_lane(lane, opt_in)?;
    if interactions::create(body)? != *create {
        return Err("create envelope differs from original request body");
    }
    if create.stream {
        return Err("native interaction streaming is not supported");
    }
    if !opt_in.pricing_modes.contains(&create.pricing_mode) {
        return Err("native service tier is not enabled");
    }
    validate_plain_text(body)?;
    match (create.previous_interaction_id.as_deref(), continuation) {
        (None, None) => {}
        (Some(id), Some(previous)) if id == previous.id && interactions::valid_id(id) => {
            validate_binding_route(lane, opt_in, previous.binding)?;
            // Previous prices and scope belong exclusively to the original run.
        }
        _ => return Err("continuation requires matching owned predecessor lookup"),
    }
    let pristine = create.model == lane.wire_model();
    let body = if pristine {
        body.to_vec()
    } else {
        let model = crate::codec::json::to_vec(&lane.wire_model())
            .map_err(|_| "cannot encode selected native model")?;
        spans::apply_top(body, &[Edit::Set("model", Cow::Borrowed(&model))])
            .ok_or("cannot splice selected native model")?
    };
    Ok(FarRequest {
        verb: "POST",
        target: "/v1beta/interactions".into(),
        fields: default_fields(lane)?,
        body,
        pristine,
        dropped_controls: Vec::new(),
    })
}

/// Prepare a stored operation only on its original, already authorized destination.
///
/// The runtime must verify the operation's ID owns `binding` before calling. This
/// helper checks route/account/model/tier consistency but does not perform lookup.
/// A changed configuration is refused; it never silently chooses a failover lane.
pub fn prepare_stored(
    lane: &Lane,
    opt_in: &NativeModel,
    binding: &Binding,
    operation: &Operation,
) -> Result<FarRequest, &'static str> {
    validate_lane(lane, opt_in)?;
    validate_binding_route(lane, opt_in, binding)?;
    if !opt_in.pricing_modes.contains(&binding.pricing_mode) {
        return Err("original native service tier is not enabled");
    }
    let (verb, id, suffix) = match operation {
        Operation::Get {
            id,
            stream: false,
            last_event_id: None,
        } => ("GET", id, ""),
        Operation::Get { .. } => return Err("native interaction streaming is not supported"),
        Operation::Cancel { id } => ("POST", id, "/cancel"),
        Operation::Delete { id } => ("DELETE", id, ""),
        Operation::Create => return Err("stored operation cannot create a paid run"),
    };
    if !interactions::valid_id(id) {
        return Err("invalid stored interaction ID");
    }
    Ok(FarRequest {
        verb,
        target: format!("/v1beta/interactions/{id}{suffix}"),
        fields: default_fields(lane)?,
        body: Vec::new(),
        pristine: true,
        dropped_controls: Vec::new(),
    })
}

/// Check a complete buffered final snapshot for the supported text-only economics.
///
/// Call on original provider bytes before redaction and alongside `interactions::normalize`.
/// Success alone proves neither usage totals, ownership, rates nor complete lifecycle.
/// The complete documented `steps` collection is required; legacy/unknown `outputs`,
/// hidden tool charges, non-text content, citations and unknown fields stay unresolved.
/// The caller must persist this validation outcome with the protected evidence before
/// submitting its final manifest, rather than reconstructing success from redacted text.
pub fn final_text_profile(body: &[u8]) -> Result<(), &'static str> {
    if body.len() > 1024 * 1024 {
        return Err("oversized native final snapshot");
    }
    // Apply the shared depth bound before the recursive typed parser. Typed structs
    // also reject duplicate keys instead of allowing last-wins accounting evidence.
    crate::codec::json::parse::<serde::de::IgnoredAny>(body)
        .map_err(|_| "malformed native final snapshot")?;
    let snapshot: TextSnapshot =
        serde_json::from_slice(body).map_err(|_| "unsupported native final snapshot shape")?;
    if !matches!(
        snapshot.status.as_str(),
        "completed" | "failed" | "cancelled" | "incomplete" | "budget_exceeded"
    ) {
        return Err("native snapshot is not terminal");
    }
    if snapshot.usage.total_tool_use_tokens != busbar_contract::Count::ZERO
        || snapshot
            .usage
            .grounding_tool_count
            .is_some_and(|rows| !rows.is_empty())
    {
        return Err("native final snapshot contains unsupported tool economics");
    }
    for step in snapshot.steps {
        let content = match step {
            TextStep::ModelOutput { content } | TextStep::UserInput { content } => content,
            TextStep::Thought { summary, .. } => summary.unwrap_or_default(),
        };
        for block in content {
            if block.kind != "text" || block.annotations.is_some_and(|rows| !rows.is_empty()) {
                return Err("native final snapshot contains unsupported content economics");
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextSnapshot {
    #[serde(rename = "id")]
    _id: String,
    #[serde(rename = "model")]
    _model: Option<String>,
    #[serde(rename = "object")]
    _object: Option<String>,
    #[serde(rename = "created")]
    _created: Option<String>,
    #[serde(rename = "updated")]
    _updated: Option<String>,
    #[serde(rename = "previous_interaction_id")]
    _previous_interaction_id: Option<String>,
    #[serde(rename = "service_tier")]
    _service_tier: Option<String>,
    #[serde(rename = "output_text")]
    _output_text: Option<String>,
    #[serde(rename = "error")]
    _error: Option<serde::de::IgnoredAny>,
    status: String,
    steps: Vec<TextStep>,
    usage: TextUsage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextUsage {
    #[serde(rename = "total_input_tokens")]
    _total_input_tokens: busbar_contract::Count,
    #[serde(rename = "total_cached_tokens")]
    _total_cached_tokens: busbar_contract::Count,
    #[serde(rename = "total_output_tokens")]
    _total_output_tokens: busbar_contract::Count,
    #[serde(rename = "total_thought_tokens")]
    _total_thought_tokens: busbar_contract::Count,
    total_tool_use_tokens: busbar_contract::Count,
    #[serde(rename = "total_tokens")]
    _total_tokens: Option<busbar_contract::Count>,
    #[serde(rename = "initial_input_tokens")]
    _initial_input_tokens: Option<busbar_contract::Count>,
    // The authoritative usage adapter validates these partitions and rejects any
    // nonzero non-text component. This shape check does not replace normalization.
    #[serde(rename = "input_tokens_by_modality")]
    _input_tokens_by_modality: Option<serde::de::IgnoredAny>,
    #[serde(rename = "cached_tokens_by_modality")]
    _cached_tokens_by_modality: Option<serde::de::IgnoredAny>,
    #[serde(rename = "output_tokens_by_modality")]
    _output_tokens_by_modality: Option<serde::de::IgnoredAny>,
    #[serde(rename = "tool_use_tokens_by_modality")]
    _tool_use_tokens_by_modality: Option<serde::de::IgnoredAny>,
    grounding_tool_count: Option<Vec<serde::de::IgnoredAny>>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TextStep {
    ModelOutput {
        content: Vec<TextContent>,
    },
    UserInput {
        content: Vec<TextContent>,
    },
    Thought {
        summary: Option<Vec<TextContent>>,
        #[serde(rename = "signature")]
        _signature: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "text")]
    _text: String,
    annotations: Option<Vec<serde::de::IgnoredAny>>,
}

fn validate_lane(lane: &Lane, opt_in: &NativeModel) -> Result<(), &'static str> {
    if lane.dialect != "gemini" || lane.invoke || lane.path.is_some() || lane.path_base.is_some() {
        return Err("native interactions require an unmodified Gemini lane");
    }
    if lane.model.is_empty() || lane.wire_model().is_empty() || lane.wire_model().len() > 256 {
        return Err("invalid selected native model");
    }
    if opt_in.account_namespace.is_empty()
        || opt_in.account_namespace.len() > 128
        || opt_in.account_namespace.trim() != opt_in.account_namespace
        || opt_in.account_namespace.chars().any(char::is_control)
    {
        return Err("invalid native account namespace");
    }
    if opt_in.pricing_modes.is_empty() || opt_in.pricing_modes.iter().any(|mode| mode != "standard")
    {
        return Err("invalid native pricing modes");
    }
    if lane.options.iter().any(|option| {
        option.place == Place::Query
            || option.invoke_only
            || attempt::governed(&option.wire)
            || option.wire.eq_ignore_ascii_case("accept")
            || option.wire.eq_ignore_ascii_case("content-type")
    }) {
        return Err("unsupported native query or governed field option");
    }
    Ok(())
}

fn validate_binding_route(
    lane: &Lane,
    opt_in: &NativeModel,
    binding: &Binding,
) -> Result<(), &'static str> {
    if binding.scope == [0; 32]
        || binding.route_entry != lane.model
        || binding.model != lane.wire_model()
        || binding.account_namespace != opt_in.account_namespace
        || binding.pricing_mode != "standard"
    {
        return Err("original interaction binding does not match selected native lane");
    }
    Ok(())
}

fn default_fields(lane: &Lane) -> Result<Vec<(String, Vec<u8>)>, &'static str> {
    let user_agent = crate::exchange::decl(lane.dialect)
        .map_or(crate::codec::protocol::EGRESS_UA_DEFAULT, |decl| {
            decl.egress_user_agent
        });
    let (mut fields, forced) = attempt::default_head_fields(
        lane,
        [
            ("content-type".into(), b"application/json".to_vec()),
            ("user-agent".into(), user_agent.as_bytes().to_vec()),
            ("accept".into(), b"application/json".to_vec()),
        ],
    );
    fields.extend(forced);
    // No caller credentials or hop-by-hop fields can enter the prepared request.
    fields.retain(|(name, _)| !attempt::governed(name));
    if fields.iter().any(|(name, value)| {
        name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || !value
                .iter()
                .all(|byte| *byte == b'\t' || (0x20..0x7f).contains(byte))
    }) {
        return Err("invalid native head field");
    }
    Ok(fields)
}

fn validate_plain_text(body: &[u8]) -> Result<(), &'static str> {
    let value: Value = crate::codec::json::parse(body).map_err(|_| "malformed native request")?;
    let object = value.as_object().ok_or("native request is not an object")?;
    let top = unique_object(body, 0)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "model"
                | "input"
                | "system_instruction"
                | "generation_config"
                | "service_tier"
                | "store"
                | "background"
                | "stream"
                | "previous_interaction_id"
        )
    }) {
        return Err("unsupported native tool, modality or request field");
    }
    if !object.get("input").is_some_and(Value::is_string)
        || object
            .get("system_instruction")
            .is_some_and(|value| !value.is_string())
    {
        return Err("native profile requires plain text input");
    }
    if let Some(generation) = object.get("generation_config") {
        let generation = generation
            .as_object()
            .ok_or("invalid native generation config")?;
        let member = top
            .last("generation_config")
            .ok_or("missing generation config span")?;
        unique_object(body, member.value_start)?;
        for (key, value) in generation {
            let valid = match key.as_str() {
                "seed" => value.as_i64().is_some(),
                "max_output_tokens" => value.as_u64().is_some_and(|n| n > 0),
                "stop_sequences" => value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string)),
                "thinking_level" => value
                    .as_str()
                    .is_some_and(|v| matches!(v, "minimal" | "low" | "medium" | "high")),
                "thinking_summaries" => {
                    value.as_str().is_some_and(|v| matches!(v, "auto" | "none"))
                }
                _ => false,
            };
            if !valid {
                return Err("unsupported native generation config");
            }
        }
    }
    Ok(())
}

fn unique_object(body: &[u8], start: usize) -> Result<spans::Object, &'static str> {
    let object = spans::members(body, start).ok_or("invalid native object span")?;
    let mut keys = BTreeSet::new();
    if object
        .members
        .iter()
        .any(|member| !keys.insert(member.key.as_str()))
    {
        return Err("duplicate native request field");
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shaping() -> Shaping {
        Shaping::from_settings(&serde_json::json!({
            "providers": {"google": {"protocol": "gemini"}},
            "models": {"entry": {"provider":"google", "upstream_model":"gemini-pinned"}}
        }))
        .unwrap()
    }
    fn opt_in() -> NativeModel {
        NativeModel {
            account_namespace: "account-a".into(),
            pricing_modes: ["standard".into()].into(),
        }
    }
    fn binding() -> Binding {
        Binding {
            scope: [1; 32],
            account_namespace: "account-a".into(),
            route_entry: "entry".into(),
            model: "gemini-pinned".into(),
            pricing_mode: "standard".into(),
            usage_schema: "gemini-interactions-2026-11-10".into(),
            input_class: 1,
            cache_read_class: 2,
            output_class: 3,
            tool_use_class: None,
            priced_classes: vec![1, 2, 3],
        }
    }
    fn prepare(body: &[u8]) -> Result<FarRequest, &'static str> {
        let tables = shaping();
        prepare_create(
            &tables.lanes["entry"],
            &opt_in(),
            &interactions::create(body)?,
            body,
            None,
        )
    }

    #[test]
    fn create_splices_only_top_level_model_and_preserves_exact_other_bytes() {
        let body = br#" { "model" : "entry", "input":"model: entry", "generation_config":{"seed":9007199254740993}, "background":true } "#;
        let request = prepare(body).unwrap();
        assert_eq!(request.body, br#" { "model" : "gemini-pinned", "input":"model: entry", "generation_config":{"seed":9007199254740993}, "background":true } "#);
        assert_eq!(request.verb, "POST");
        assert_eq!(request.target, "/v1beta/interactions");
        assert!(!request.pristine);
        assert!(request.dropped_controls.is_empty());
        assert!(request
            .fields
            .contains(&("accept".into(), b"application/json".to_vec())));
        assert!(request
            .fields
            .contains(&("content-type".into(), b"application/json".to_vec())));
    }

    #[test]
    fn matching_wire_model_keeps_body_pristine_and_envelope_mismatch_is_refused() {
        let body = br#"{"model":"gemini-pinned", "input":"hello"}"#;
        let request = prepare(body).unwrap();
        assert_eq!(request.body, body);
        assert!(request.pristine);
        let mut create = interactions::create(body).unwrap();
        create.pricing_mode = "flex".into();
        assert!(prepare_create(&shaping().lanes["entry"], &opt_in(), &create, body, None).is_err());
    }

    #[test]
    fn native_opt_in_is_disabled_by_default_and_only_validates_named_models() {
        let empty: NativeConfig = serde_json::from_str("{}").unwrap();
        assert!(empty.models.is_empty());
        empty.validate(&Shaping::default()).unwrap();
        let mut config = NativeConfig {
            models: [("entry".into(), opt_in())].into(),
        };
        config.validate(&shaping()).unwrap();
        config.models.insert("unknown".into(), opt_in());
        assert!(config.validate(&shaping()).is_err());
        assert!(serde_json::from_str::<NativeConfig>(r#"{"enabled":true}"#).is_err());
        assert!(serde_json::from_str::<NativeModel>(
            r#"{"account_namespace":"a","pricing_modes":["standard"],"free":true}"#
        )
        .is_err());
    }

    #[test]
    fn unsupported_lanes_overrides_and_account_tiers_fail_before_preparation() {
        let lane = shaping().lanes["entry"].clone();
        for change in 0..4 {
            let mut changed = lane.clone();
            match change {
                0 => changed.dialect = "responses",
                1 => changed.invoke = true,
                2 => changed.path = Some("/v1beta/interactions".into()),
                _ => changed.path_base = Some(String::new()),
            }
            assert!(validate_lane(&changed, &opt_in()).is_err());
        }
        for modes in [BTreeSet::new(), ["unknown".into()].into()] {
            assert!(validate_lane(
                &lane,
                &NativeModel {
                    account_namespace: "a".into(),
                    pricing_modes: modes
                }
            )
            .is_err());
        }
        let mut invalid = opt_in();
        invalid.account_namespace.clear();
        assert!(validate_lane(&lane, &invalid).is_err());
        assert!(prepare(br#"{"model":"entry","input":"hi","service_tier":"priority"}"#).is_err());
    }

    #[test]
    fn tool_and_modality_variants_and_duplicate_keys_never_dispatch() {
        for body in [
            br#"{"model":"entry","input":"hi","tools":[]}"#.as_slice(),
            br#"{"model":"entry","input":[{"type":"image","data":"x"}]}"#,
            br#"{"model":"entry","input":"hi","response_format":{"type":"image"}}"#,
            br#"{"model":"entry","input":"hi","generation_config":{"tool_choice":"auto"}}"#,
            br#"{"model":"entry","input":"hi","environment":"remote-1"}"#,
            br#"{"model":"entry","input":"hi","model":"other"}"#,
            br#"{"model":"entry","input":"hi","m\u006fdel":"entry"}"#,
            br#"{"model":"entry","input":"hi","generation_config":{"seed":1,"seed":2}}"#,
        ] {
            assert!(prepare(body).is_err(), "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn stream_intent_is_explicitly_refused_and_never_silently_buffered() {
        assert!(prepare(br#"{"model":"entry","input":"hi","stream":true}"#).is_err());
        for operation in [
            Operation::Get {
                id: "run-1".into(),
                stream: true,
                last_event_id: Some("evt-1".into()),
            },
            Operation::Get {
                id: "run-1".into(),
                stream: false,
                last_event_id: Some("evt-1".into()),
            },
        ] {
            assert!(
                prepare_stored(&shaping().lanes["entry"], &opt_in(), &binding(), &operation)
                    .is_err()
            );
        }
    }

    #[test]
    fn stored_targets_are_canonical_and_ids_cannot_inject_paths_or_queries() {
        let lane = &shaping().lanes["entry"];
        for (operation, verb, target) in [
            (
                Operation::Get {
                    id: "run-1".into(),
                    stream: false,
                    last_event_id: None,
                },
                "GET",
                "/v1beta/interactions/run-1",
            ),
            (
                Operation::Cancel { id: "run-1".into() },
                "POST",
                "/v1beta/interactions/run-1/cancel",
            ),
            (
                Operation::Delete { id: "run-1".into() },
                "DELETE",
                "/v1beta/interactions/run-1",
            ),
        ] {
            let request = prepare_stored(lane, &opt_in(), &binding(), &operation).unwrap();
            assert_eq!((request.verb, request.target.as_str()), (verb, target));
            assert!(request.body.is_empty());
        }
        for id in ["../escape", "x?stream=true", "x%2Fy", "..", ""] {
            assert!(prepare_stored(
                lane,
                &opt_in(),
                &binding(),
                &Operation::Delete { id: id.into() }
            )
            .is_err());
        }
        assert!(prepare_stored(lane, &opt_in(), &binding(), &Operation::Create).is_err());
    }

    #[test]
    fn stored_binding_cannot_change_account_route_model_or_tier() {
        let operation = Operation::Delete { id: "run-1".into() };
        for change in 0..5 {
            let mut original = binding();
            match change {
                0 => original.account_namespace = "other".into(),
                1 => original.route_entry = "other".into(),
                2 => original.model = "other".into(),
                3 => original.pricing_mode = "priority".into(),
                _ => original.scope = [0; 32],
            }
            assert!(
                prepare_stored(&shaping().lanes["entry"], &opt_in(), &original, &operation)
                    .is_err()
            );
        }
    }

    #[test]
    fn continuation_requires_owned_matching_predecessor_without_reusing_its_scope() {
        let body = br#"{"model":"entry","input":"next","previous_interaction_id":"run-1","service_tier":"standard"}"#;
        let create = interactions::create(body).unwrap();
        let original = binding();
        let tables = shaping();
        let lane = &tables.lanes["entry"];
        assert!(prepare_create(lane, &opt_in(), &create, body, None).is_err());
        assert!(prepare_create(
            lane,
            &opt_in(),
            &create,
            body,
            Some(Continuation {
                id: "other",
                binding: &original
            })
        )
        .is_err());
        let request = prepare_create(
            lane,
            &opt_in(),
            &create,
            body,
            Some(Continuation {
                id: "run-1",
                binding: &original,
            }),
        )
        .unwrap();
        let value: Value = crate::codec::json::parse(&request.body).unwrap();
        assert_eq!(value["service_tier"], "standard");
        assert_eq!(value["previous_interaction_id"], "run-1");
        let mut other = original.clone();
        other.account_namespace = "other".into();
        assert!(prepare_create(
            lane,
            &opt_in(),
            &create,
            body,
            Some(Continuation {
                id: "run-1",
                binding: &other
            })
        )
        .is_err());
    }

    #[test]
    fn nonstandard_tiers_are_refused_without_an_explicit_host_class_mapping() {
        for mode in ["priority", "flex"] {
            let mut configured = opt_in();
            configured.pricing_modes.insert(mode.into());
            assert!(validate_lane(&shaping().lanes["entry"], &configured).is_err());
            let body = format!(r#"{{"model":"entry","input":"hi","service_tier":"{mode}"}}"#);
            assert!(prepare(body.as_bytes()).is_err());
        }
    }

    fn final_snapshot() -> Value {
        serde_json::json!({
            "id":"run-1", "model":"gemini-pinned", "status":"completed",
            "steps":[
                {"type":"user_input","content":[{"type":"text","text":"question"}]},
                {"type":"thought","signature":"sig","summary":[{"type":"text","text":"reasoning"}]},
                {"type":"model_output","content":[{"type":"text","text":"answer"}]}
            ],
            "usage":{"total_input_tokens":20,"total_cached_tokens":2,"total_output_tokens":5,
                "total_thought_tokens":3,"total_tool_use_tokens":0}
        })
    }

    #[test]
    fn final_text_profile_accepts_documented_text_steps_and_empty_terminal_failure() {
        let mut snapshot = final_snapshot();
        final_text_profile(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        snapshot["steps"] = serde_json::json!([]);
        snapshot["status"] = "failed".into();
        final_text_profile(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        snapshot["status"] = "in_progress".into();
        assert!(final_text_profile(&serde_json::to_vec(&snapshot).unwrap()).is_err());
    }

    #[test]
    fn final_text_profile_refuses_tool_and_nontext_economics_even_with_zero_usage() {
        for change in 0..8 {
            let mut snapshot = final_snapshot();
            match change {
                0 => {
                    snapshot["steps"][2] =
                        serde_json::json!({"type":"code_execution_call","arguments":{}})
                }
                1 => {
                    snapshot["steps"][2]["content"][0] =
                        serde_json::json!({"type":"image","data":"x"})
                }
                2 => {
                    snapshot["steps"][1]["summary"][0] =
                        serde_json::json!({"type":"audio","data":"x"})
                }
                3 => snapshot["usage"]["total_tool_use_tokens"] = 1.into(),
                4 => {
                    snapshot["usage"]["grounding_tool_count"] =
                        serde_json::json!([{"type":"google_search","count":1}])
                }
                5 => {
                    snapshot["steps"][2]["content"][0]["annotations"] =
                        serde_json::json!([{"type":"url_citation"}])
                }
                6 => {
                    snapshot.as_object_mut().unwrap().remove("steps");
                }
                _ => snapshot["outputs"] = serde_json::json!([]),
            }
            assert!(
                final_text_profile(&serde_json::to_vec(&snapshot).unwrap()).is_err(),
                "change {change}"
            );
        }
    }

    #[test]
    fn final_text_profile_refuses_duplicate_type_and_usage_fields() {
        let snapshot = serde_json::to_string(&final_snapshot()).unwrap();
        for altered in [
            snapshot.replace(
                "\"type\":\"model_output\"",
                "\"type\":\"code_execution_call\",\"type\":\"model_output\"",
            ),
            snapshot.replace(
                "\"total_tool_use_tokens\":0",
                "\"total_tool_use_tokens\":1,\"total_tool_use_tokens\":0",
            ),
            snapshot.replace("\"steps\":[", "\"steps\":[],\"steps\":["),
        ] {
            assert!(final_text_profile(altered.as_bytes()).is_err());
        }
    }

    #[test]
    fn lane_options_cannot_override_native_targets_or_framing() {
        for (wire, place) in [
            ("alt", Place::Query),
            ("authorization", Place::Header),
            ("accept", Place::Header),
            ("content-type", Place::Header),
        ] {
            let mut lane = shaping().lanes["entry"].clone();
            lane.options.push(crate::options::SetOption {
                wire: wire.into(),
                place,
                value: "x".into(),
                precedence: crate::options::Precedence::Forced,
                invoke_only: false,
            });
            assert!(validate_lane(&lane, &opt_in()).is_err());
        }
    }

    #[test]
    fn configured_header_defaults_keep_credentials_out_and_reject_injection() {
        let mut lane = shaping().lanes["entry"].clone();
        lane.statics = &[("x-goog-api-key", "secret"), ("x-native-version", "v1")];
        let fields = default_fields(&lane).unwrap();
        assert!(!fields.iter().any(|(name, _)| name == "x-goog-api-key"));
        assert!(fields.contains(&("x-native-version".into(), b"v1".to_vec())));
        lane.statics = &[("x-native-version", "v1\r\nx-evil: yes")];
        assert!(default_fields(&lane).is_err());
    }
}
