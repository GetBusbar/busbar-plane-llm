// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Bounded, sans-I/O Gemini Interactions SSE and text-economics tracking.
//!
//! Raw frames retain their delimiters and bytes. JSON is inspected through the
//! exact-number codec; the final interaction is a byte slice of provider event data,
//! never a synthesized response. Event IDs are replay cursors, never revisions.
//! Ownership, durable early-ID publication, final usage normalization and settlement
//! remain the runtime's responsibility. A proof is available only at clean EOF.
//!
//! Shapes checked against <https://aistudio.google.com/api-reference/interactions-api>
//! on 2026-11-10: `interaction.completed` carries a partial interaction with optional
//! steps; `interaction.status_update` has `interaction_id`, not a complete resource.
//! Missing history in a resumed stream cannot prove absent tool/media economics.

use super::{interactions::valid_id, native_request::final_text_profile};
use busbar_contract::spans;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Maximum pending frame and maximum input chunk, each independently bounded.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum structured events inspected during one stream.
pub const MAX_EVENTS: usize = 4096;
/// Maximum indexed steps tracked during one stream.
pub const MAX_STEPS: usize = 1024;

/// Whether this request starts at the beginning or resumes after a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// Create or retrieval without a cursor; observed proof still requires created/start/stop.
    Fresh,
    /// Omitted history is unknown, even if all received deltas are text.
    Replay,
}

/// Content-free reason a stream cannot establish complete supported economics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamIssue {
    /// A configured/expected provider ID is invalid or absent for a replay.
    InvalidExpectedId,
    /// Input chunk or pending frame exceeds the published byte bound.
    SizeLimit,
    /// Structured event, event identity or step bound exceeded.
    StateLimit,
    /// The original frame is not UTF-8; bytes are never replaced lossily.
    InvalidUtf8,
    /// Invalid or ambiguous SSE metadata or JSON structure.
    MalformedEvent,
    /// A repeated event identity cannot establish a new trustworthy observation.
    DuplicateEventId,
    /// A provider interaction ID changed or differs from the authenticated original.
    IdentityMismatch,
    /// Reported model changed within the stream.
    ModelMismatch,
    /// Unknown/tool/media event, step, delta or field.
    UnsupportedEvent,
    /// Unsupported tool/modality usage or malformed counter evidence.
    UnsupportedUsage,
    /// Repeated/inconsistent start/stop or events after completion.
    InvalidSequence,
    /// A final interaction lacks complete usage facts.
    MissingUsage,
    /// No authoritative completed interaction was received.
    MissingTerminal,
    /// Fresh-stream start/stop history is incomplete.
    IncompleteHistory,
    /// A cursor excluded history and final evidence has no independent full steps.
    ReplayHistoryMissing,
    /// The terminal full resource does not meet the supported text profile.
    UnsupportedSnapshot,
    /// EOF arrived inside a frame; even a prior terminal cannot close the scope.
    CutFrame,
}

/// How supported final economics were established, independent of host authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    /// The final resource explicitly includes its supported full steps.
    FullSnapshot,
    /// The entire fresh stream was observed with complete text/thought step lifecycles.
    ObservedFreshStream,
}

/// Final profile outcome; unresolved never means a successful zero charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ProfileVerdict {
    /// Additional final usage and host checks may proceed.
    Supported(ProofKind),
    /// Retain the scope and reconcile with authoritative evidence later.
    Unresolved(StreamIssue),
}

/// Bounded diagnostics containing only fixed type names and counters, never generated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDiagnostics {
    /// Fresh or cursor-resumed request mode.
    pub mode: StreamMode,
    /// Complete frames, including comment/keepalive frames.
    pub frames: u64,
    /// Structured events examined, bounded by MAX_EVENTS.
    pub events: u32,
    /// Recognized event kinds, from a fixed vocabulary only.
    pub event_types: BTreeSet<String>,
    /// Recognized text/thought step kinds only.
    pub step_types: BTreeSet<String>,
    /// Recognized text/thought delta kinds only.
    pub delta_types: BTreeSet<String>,
    /// Steps whose starts and stops were both observed.
    pub completed_steps: u32,
    /// A created event identified the provider object.
    pub saw_created: bool,
    /// An interaction.completed resource was received.
    pub saw_completed: bool,
    /// Sticky first problem, if any.
    pub issue: Option<StreamIssue>,
}

/// One complete, untouched SSE frame, ready for runtime-controlled forwarding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFrame {
    /// Original bytes including the exact CR/LF frame separator.
    pub wire: Vec<u8>,
    /// First validated created ID; publish its durable binding before forwarding.
    pub created_id: Option<String>,
    /// Hold this frame and all following bytes until EOF/settlement.
    pub terminal: bool,
    /// Sticky profile issue visible at this frame, without modifying its wire bytes.
    pub issue: Option<StreamIssue>,
}

/// Original authoritative resource, extracted without reserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    /// Exact interaction object from the provider's SSE data payload.
    pub interaction: Vec<u8>,
}

/// EOF result. Forward `frames` then `tail` in order to preserve all original bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCompletion {
    /// A bare-CR frame delimiter may become complete only at EOF.
    pub frames: Vec<StreamFrame>,
    /// Unfinished raw frame bytes; nonempty means CutFrame and no supported proof.
    pub tail: Vec<u8>,
    /// Original final resource, if received; never synthesized from a status update.
    pub terminal: Option<Terminal>,
    /// Final supported or explicit unresolved profile.
    pub profile: ProfileVerdict,
    /// Persist atomically with the protected final resource before settlement.
    pub diagnostics: ProfileDiagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Text,
    Thought,
}

/// Streaming tokenizer plus bounded provider-specific economic-profile tracker.
pub struct NativeStream {
    pending: Vec<u8>,
    line_has_content: bool,
    pending_cr: bool,
    provider_id: Option<String>,
    reported_model: Option<String>,
    reported_tier: Option<String>,
    event_ids: BTreeSet<String>,
    steps: BTreeMap<u32, (StepKind, bool)>,
    next_step: u32,
    history_complete: bool,
    hold: bool,
    terminal: Option<Terminal>,
    final_has_steps: bool,
    diagnostics: ProfileDiagnostics,
}

impl NativeStream {
    /// Start under runtime-authenticated request mode and original provider identity.
    /// A replay requires the ID obtained by durable caller-scoped lookup.
    pub fn new(mode: StreamMode, expected_id: Option<String>) -> Result<Self, StreamIssue> {
        if expected_id.as_deref().is_some_and(|id| !valid_id(id))
            || (mode == StreamMode::Replay && expected_id.is_none())
        {
            return Err(StreamIssue::InvalidExpectedId);
        }
        Ok(Self {
            pending: Vec::new(),
            line_has_content: false,
            pending_cr: false,
            provider_id: expected_id,
            reported_model: None,
            reported_tier: None,
            event_ids: BTreeSet::new(),
            steps: BTreeMap::new(),
            next_step: 0,
            history_complete: mode == StreamMode::Fresh,
            hold: false,
            terminal: None,
            final_has_steps: false,
            diagnostics: ProfileDiagnostics {
                mode,
                frames: 0,
                events: 0,
                event_types: BTreeSet::new(),
                step_types: BTreeSet::new(),
                delta_types: BTreeSet::new(),
                completed_steps: 0,
                saw_created: false,
                saw_completed: false,
                issue: None,
            },
        })
    }

    /// Consume one chunk exactly once. On size refusal stop ingestion; no proof is possible.
    /// Semantic failures still return original frames with explicit unresolved diagnostics.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamFrame>, StreamIssue> {
        if chunk.len() > MAX_FRAME_BYTES {
            self.fail(StreamIssue::SizeLimit);
            return Err(StreamIssue::SizeLimit);
        }
        let mut frames = Vec::new();
        for &byte in chunk {
            // Bound returned frame-object overhead even for a chunk of only empty lines.
            if frames.len() == MAX_EVENTS {
                self.fail(StreamIssue::StateLimit);
                return Err(StreamIssue::StateLimit);
            }
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    self.append(byte)?;
                    self.end_line(&mut frames);
                    continue;
                }
                self.end_line(&mut frames);
            }
            self.append(byte)?;
            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' => self.end_line(&mut frames),
                _ => self.line_has_content = true,
            }
        }
        Ok(frames)
    }

    /// Finish at transport EOF. Truncation or trailing semantic failures prevent proof.
    pub fn finish(mut self) -> StreamCompletion {
        let mut frames = Vec::new();
        if self.pending_cr {
            self.pending_cr = false;
            self.end_line(&mut frames);
        }
        if !self.pending.is_empty() {
            self.fail(StreamIssue::CutFrame);
        }
        let profile = if let Some(issue) = self.diagnostics.issue {
            ProfileVerdict::Unresolved(issue)
        } else if let Some(terminal) = &self.terminal {
            if self.final_has_steps {
                match final_text_profile(&terminal.interaction) {
                    Ok(()) => ProfileVerdict::Supported(ProofKind::FullSnapshot),
                    Err(_) => ProfileVerdict::Unresolved(StreamIssue::UnsupportedSnapshot),
                }
            } else if self.diagnostics.mode == StreamMode::Replay {
                ProfileVerdict::Unresolved(StreamIssue::ReplayHistoryMissing)
            } else if !self.history_complete
                || !self.diagnostics.saw_created
                || self.steps.is_empty()
                || self.steps.values().any(|(_, closed)| !closed)
            {
                ProfileVerdict::Unresolved(StreamIssue::IncompleteHistory)
            } else {
                ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
            }
        } else {
            ProfileVerdict::Unresolved(StreamIssue::MissingTerminal)
        };
        if let ProfileVerdict::Unresolved(issue) = profile {
            self.fail(issue);
        }
        StreamCompletion {
            frames,
            tail: self.pending,
            terminal: self.terminal,
            profile,
            diagnostics: self.diagnostics,
        }
    }

    fn fail(&mut self, issue: StreamIssue) {
        self.diagnostics.issue.get_or_insert(issue);
    }
    fn append(&mut self, byte: u8) -> Result<(), StreamIssue> {
        if self.pending.len() == MAX_FRAME_BYTES {
            self.fail(StreamIssue::SizeLimit);
            return Err(StreamIssue::SizeLimit);
        }
        self.pending.push(byte);
        Ok(())
    }
    fn end_line(&mut self, frames: &mut Vec<StreamFrame>) {
        if self.line_has_content {
            self.line_has_content = false;
            return;
        }
        let wire = std::mem::take(&mut self.pending);
        self.diagnostics.frames = self.diagnostics.frames.saturating_add(1);
        let created_id = match self.inspect(&wire) {
            Ok(id) => id,
            Err(issue) => {
                self.fail(issue);
                None
            }
        };
        frames.push(StreamFrame {
            wire,
            created_id,
            terminal: self.hold,
            issue: self.diagnostics.issue,
        });
    }

    fn inspect(&mut self, wire: &[u8]) -> Result<Option<String>, StreamIssue> {
        let frame = parse_frame(wire)?;
        let Some(data) = frame.data else {
            return Ok(None);
        };
        if self.diagnostics.events as usize == MAX_EVENTS {
            return Err(StreamIssue::StateLimit);
        }
        self.diagnostics.events += 1;
        let root: Value =
            crate::codec::json::parse(&data).map_err(|_| StreamIssue::MalformedEvent)?;
        crate::codec::json::validate_unique_json(&data).map_err(|_| StreamIssue::MalformedEvent)?;
        let object = root.as_object().ok_or(StreamIssue::MalformedEvent)?;
        let kind = object
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or(StreamIssue::MalformedEvent)?;
        if frame.event.as_deref().is_some_and(|event| event != kind) {
            return Err(StreamIssue::MalformedEvent);
        }
        if kind == "interaction.completed" {
            self.hold = true;
        }
        let event_id = object
            .get("event_id")
            .map(|value| value.as_str().ok_or(StreamIssue::MalformedEvent))
            .transpose()?;
        if frame
            .id
            .as_deref()
            .zip(event_id)
            .is_some_and(|(a, b)| a != b)
        {
            return Err(StreamIssue::MalformedEvent);
        }
        if let Some(id) = event_id.or(frame.id.as_deref()) {
            if !valid_id(id) {
                return Err(StreamIssue::MalformedEvent);
            }
            if !self.event_ids.insert(id.to_owned()) {
                return Err(StreamIssue::DuplicateEventId);
            }
        }
        if self.terminal.is_some() {
            return Err(StreamIssue::InvalidSequence);
        }
        match kind {
            "interaction.created" | "interaction.completed" => {
                only_keys(&root, &["event_type", "event_id", "interaction"])?;
                self.diagnostics.event_types.insert(kind.into());
                let interaction = root.get("interaction").ok_or(StreamIssue::MalformedEvent)?;
                let id = self.identity(interaction.get("id"))?.to_owned();
                self.model(interaction.get("model"))?;
                self.tier(interaction.get("service_tier"))?;
                interaction_fields(interaction)?;
                let span = spans::members(&data, 0)
                    .and_then(|members| members.last("interaction").cloned())
                    .ok_or(StreamIssue::MalformedEvent)?;
                if kind == "interaction.created" {
                    if self.diagnostics.saw_created {
                        return Err(StreamIssue::InvalidSequence);
                    }
                    if self.diagnostics.events != 1 {
                        self.history_complete = false;
                    }
                    self.diagnostics.saw_created = true;
                    if interaction.get("status").and_then(Value::as_str) != Some("in_progress") {
                        return Err(StreamIssue::InvalidSequence);
                    }
                    if let Some(steps) = interaction.get("steps") {
                        if steps.as_array().is_none_or(|items| !items.is_empty()) {
                            return Err(StreamIssue::UnsupportedEvent);
                        }
                    }
                    if let Some(usage) = interaction.get("usage") {
                        usage_profile(usage, false)?;
                    }
                    Ok(Some(id))
                } else {
                    if !terminal_status(interaction.get("status").and_then(Value::as_str)) {
                        return Err(StreamIssue::InvalidSequence);
                    }
                    self.diagnostics.saw_completed = true;
                    self.final_has_steps = interaction.get("steps").is_some();
                    self.terminal = Some(Terminal {
                        interaction: data[span.value_start..span.value_end].to_vec(),
                    });
                    usage_profile(
                        interaction.get("usage").ok_or(StreamIssue::MissingUsage)?,
                        true,
                    )?;
                    Ok(None)
                }
            }
            "interaction.status_update" => {
                only_keys(
                    &root,
                    &[
                        "event_type",
                        "event_id",
                        "interaction_id",
                        "status",
                        "usage",
                    ],
                )?;
                self.diagnostics.event_types.insert(kind.into());
                self.identity(root.get("interaction_id"))?;
                let status = root
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or(StreamIssue::MalformedEvent)?;
                if terminal_status(Some(status)) {
                    self.hold = true;
                } else if status != "in_progress" {
                    return Err(StreamIssue::UnsupportedEvent);
                }
                if let Some(usage) = root.get("usage") {
                    usage_profile(usage, false)?;
                }
                Ok(None)
            }
            "step.start" | "step.delta" | "step.stop" => {
                self.diagnostics.event_types.insert(kind.into());
                if !self.diagnostics.saw_created {
                    self.history_complete = false;
                }
                let index = root
                    .get("index")
                    .and_then(Value::as_u64)
                    .filter(|index| *index < MAX_STEPS as u64)
                    .ok_or(StreamIssue::StateLimit)? as u32;
                match kind {
                    "step.start" => {
                        only_keys(&root, &["event_type", "event_id", "index", "step"])?;
                        let step = root.get("step").ok_or(StreamIssue::MalformedEvent)?;
                        let (step_kind, name) = step_profile(step)?;
                        self.diagnostics.step_types.insert(name.into());
                        if self.steps.insert(index, (step_kind, false)).is_some() {
                            return Err(StreamIssue::InvalidSequence);
                        }
                        if index != self.next_step {
                            self.history_complete = false;
                        }
                        self.next_step = index + 1;
                    }
                    "step.delta" => {
                        only_keys(
                            &root,
                            &["event_type", "event_id", "index", "delta", "metadata"],
                        )?;
                        let delta = root.get("delta").ok_or(StreamIssue::MalformedEvent)?;
                        let (delta_kind, name) = delta_profile(delta)?;
                        self.diagnostics.delta_types.insert(name.into());
                        match self.steps.get(&index) {
                            Some((step_kind, false)) if *step_kind == delta_kind => {}
                            Some(_) => return Err(StreamIssue::InvalidSequence),
                            None => self.history_complete = false,
                        }
                        if let Some(metadata) = root.get("metadata") {
                            only_keys(metadata, &["total_usage"])?;
                            if let Some(usage) = metadata.get("total_usage") {
                                usage_profile(usage, false)?;
                            }
                        }
                    }
                    _ => {
                        only_keys(
                            &root,
                            &["event_type", "event_id", "index", "usage", "step_usage"],
                        )?;
                        match self.steps.get_mut(&index) {
                            Some((_, closed)) if !*closed => {
                                *closed = true;
                                self.diagnostics.completed_steps += 1;
                            }
                            Some(_) => return Err(StreamIssue::InvalidSequence),
                            None => self.history_complete = false,
                        }
                        for field in ["usage", "step_usage"] {
                            if let Some(usage) = root.get(field) {
                                usage_profile(usage, false)?;
                            }
                        }
                    }
                }
                Ok(None)
            }
            _ => Err(StreamIssue::UnsupportedEvent),
        }
    }

    fn identity(&mut self, value: Option<&Value>) -> Result<&str, StreamIssue> {
        let id = value
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or(StreamIssue::MalformedEvent)?;
        if self
            .provider_id
            .as_deref()
            .is_some_and(|original| original != id)
        {
            return Err(StreamIssue::IdentityMismatch);
        }
        self.provider_id.get_or_insert_with(|| id.to_owned());
        Ok(self.provider_id.as_deref().unwrap())
    }
    fn model(&mut self, value: Option<&Value>) -> Result<(), StreamIssue> {
        if let Some(value) = value {
            let model = value
                .as_str()
                .filter(|model| !model.is_empty() && model.len() <= 256)
                .ok_or(StreamIssue::MalformedEvent)?;
            if self
                .reported_model
                .as_deref()
                .is_some_and(|original| original != model)
            {
                return Err(StreamIssue::ModelMismatch);
            }
            self.reported_model.get_or_insert_with(|| model.to_owned());
        }
        Ok(())
    }
    fn tier(&mut self, value: Option<&Value>) -> Result<(), StreamIssue> {
        if let Some(value) = value {
            let tier = value.as_str().ok_or(StreamIssue::UnsupportedUsage)?;
            super::profiles::text_classes(tier).map_err(|_| StreamIssue::UnsupportedUsage)?;
            if self
                .reported_tier
                .as_deref()
                .is_some_and(|original| original != tier)
            {
                return Err(StreamIssue::UnsupportedUsage);
            }
            self.reported_tier.get_or_insert_with(|| tier.to_owned());
        }
        Ok(())
    }
}

struct ParsedFrame {
    data: Option<Vec<u8>>,
    event: Option<String>,
    id: Option<String>,
}
fn parse_frame(wire: &[u8]) -> Result<ParsedFrame, StreamIssue> {
    std::str::from_utf8(wire).map_err(|_| StreamIssue::InvalidUtf8)?;
    let mut frame = ParsedFrame {
        data: None,
        event: None,
        id: None,
    };
    let mut retry = false;
    for (start, end) in crate::codec::protocol::sse_line_spans(wire) {
        let line = &wire[start..end];
        if line.is_empty() || line.starts_with(b":") {
            continue;
        }
        let (field, value) = line
            .iter()
            .position(|byte| *byte == b':')
            .map_or((line, &b""[..]), |at| (&line[..at], &line[at + 1..]));
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"data" => match &mut frame.data {
                None => frame.data = Some(value.to_vec()),
                Some(data) => {
                    data.push(b'\n');
                    data.extend_from_slice(value);
                }
            },
            b"event" if frame.event.is_none() => {
                frame.event = Some(std::str::from_utf8(value).unwrap().to_owned())
            }
            b"id" if frame.id.is_none() && !value.contains(&0) => {
                frame.id = Some(std::str::from_utf8(value).unwrap().to_owned())
            }
            b"retry" if !retry && !value.is_empty() && value.iter().all(u8::is_ascii_digit) => {
                retry = true
            }
            _ => return Err(StreamIssue::MalformedEvent),
        }
    }
    if frame.data.is_none() && (frame.event.is_some() || frame.id.is_some()) {
        return Err(StreamIssue::MalformedEvent);
    }
    Ok(frame)
}

fn only_keys(value: &Value, allowed: &[&str]) -> Result<(), StreamIssue> {
    let object = value.as_object().ok_or(StreamIssue::MalformedEvent)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(StreamIssue::UnsupportedEvent);
    }
    Ok(())
}
fn terminal_status(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed" | "failed" | "cancelled" | "incomplete" | "budget_exceeded")
    )
}
fn interaction_fields(value: &Value) -> Result<(), StreamIssue> {
    only_keys(
        value,
        &[
            "id",
            "status",
            "model",
            "object",
            "created",
            "updated",
            "steps",
            "usage",
            "service_tier",
            "previous_interaction_id",
            "output_text",
            "error",
        ],
    )?;
    if value.get("service_tier").is_some_and(|tier| {
        tier.as_str()
            .is_none_or(|tier| super::profiles::text_classes(tier).is_err())
    }) {
        return Err(StreamIssue::UnsupportedUsage);
    }
    Ok(())
}
fn text_content(value: &Value) -> Result<(), StreamIssue> {
    only_keys(value, &["type", "text", "annotations"])?;
    if value.get("type").and_then(Value::as_str) != Some("text")
        || !value.get("text").is_some_and(Value::is_string)
        || value
            .get("annotations")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err(StreamIssue::UnsupportedEvent);
    }
    Ok(())
}
fn content_array(value: &Value) -> Result<(), StreamIssue> {
    for content in value.as_array().ok_or(StreamIssue::MalformedEvent)? {
        text_content(content)?;
    }
    Ok(())
}
fn step_profile(value: &Value) -> Result<(StepKind, &'static str), StreamIssue> {
    match value.get("type").and_then(Value::as_str) {
        Some("model_output") => {
            only_keys(value, &["type", "content"])?;
            if let Some(content) = value.get("content") {
                content_array(content)?;
            }
            Ok((StepKind::Text, "model_output"))
        }
        Some("thought") => {
            only_keys(value, &["type", "summary", "signature"])?;
            if let Some(summary) = value.get("summary") {
                content_array(summary)?;
            }
            if value.get("signature").is_some_and(|v| !v.is_string()) {
                return Err(StreamIssue::MalformedEvent);
            }
            Ok((StepKind::Thought, "thought"))
        }
        _ => Err(StreamIssue::UnsupportedEvent),
    }
}
fn delta_profile(value: &Value) -> Result<(StepKind, &'static str), StreamIssue> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => {
            text_content(value)?;
            Ok((StepKind::Text, "text"))
        }
        Some("thought_summary") => {
            only_keys(value, &["type", "content"])?;
            text_content(value.get("content").ok_or(StreamIssue::MalformedEvent)?)?;
            Ok((StepKind::Thought, "thought_summary"))
        }
        Some("thought_signature") => {
            only_keys(value, &["type", "signature"])?;
            if value.get("signature").is_some_and(|v| !v.is_string()) {
                return Err(StreamIssue::MalformedEvent);
            }
            Ok((StepKind::Thought, "thought_signature"))
        }
        _ => Err(StreamIssue::UnsupportedEvent),
    }
}
fn usage_profile(usage: &Value, complete: bool) -> Result<(), StreamIssue> {
    only_keys(
        usage,
        &[
            "total_input_tokens",
            "initial_input_tokens",
            "total_cached_tokens",
            "total_output_tokens",
            "total_thought_tokens",
            "total_tool_use_tokens",
            "total_tokens",
            "input_tokens_by_modality",
            "cached_tokens_by_modality",
            "output_tokens_by_modality",
            "tool_use_tokens_by_modality",
            "grounding_tool_count",
        ],
    )
    .map_err(|_| StreamIssue::UnsupportedUsage)?;
    let required = [
        "total_input_tokens",
        "total_cached_tokens",
        "total_output_tokens",
        "total_thought_tokens",
        "total_tool_use_tokens",
    ];
    for field in required
        .into_iter()
        .chain(["initial_input_tokens", "total_tokens"])
    {
        match usage.get(field) {
            Some(value) => {
                let count = exact_integer(value)?;
                if field == "total_tool_use_tokens" && count != 0 {
                    return Err(StreamIssue::UnsupportedUsage);
                }
            }
            None if complete && required.contains(&field) => return Err(StreamIssue::MissingUsage),
            None => {}
        }
    }
    if usage
        .get("grounding_tool_count")
        .is_some_and(|rows| rows.as_array().is_none_or(|rows| !rows.is_empty()))
    {
        return Err(StreamIssue::UnsupportedUsage);
    }
    for field in [
        "input_tokens_by_modality",
        "cached_tokens_by_modality",
        "output_tokens_by_modality",
        "tool_use_tokens_by_modality",
    ] {
        if let Some(rows) = usage.get(field) {
            let rows = rows
                .as_array()
                .filter(|rows| rows.len() <= 5)
                .ok_or(StreamIssue::UnsupportedUsage)?;
            let mut seen = BTreeSet::new();
            for row in rows {
                only_keys(row, &["modality", "tokens"])
                    .map_err(|_| StreamIssue::UnsupportedUsage)?;
                let modality = row
                    .get("modality")
                    .and_then(Value::as_str)
                    .ok_or(StreamIssue::UnsupportedUsage)?;
                if !matches!(modality, "text" | "image" | "audio" | "video" | "document")
                    || !seen.insert(modality)
                {
                    return Err(StreamIssue::UnsupportedUsage);
                }
                let count = exact_integer(row.get("tokens").ok_or(StreamIssue::UnsupportedUsage)?)?;
                if modality != "text" && count != 0 {
                    return Err(StreamIssue::UnsupportedUsage);
                }
            }
        }
    }
    Ok(())
}
fn exact_integer(value: &Value) -> Result<u64, StreamIssue> {
    crate::codec::usage_count::read_count(value)
        .ok_or(StreamIssue::UnsupportedUsage)?
        .whole_u64()
        .map_err(|_| StreamIssue::UnsupportedUsage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created() -> Value {
        serde_json::json!({"event_type":"interaction.created","event_id":"e-created","interaction":{"id":"run-1","model":"gemini-pinned","status":"in_progress"}})
    }
    fn start() -> Value {
        serde_json::json!({"event_type":"step.start","event_id":"e-start","index":0,"step":{"type":"model_output"}})
    }
    fn delta() -> Value {
        serde_json::json!({"event_type":"step.delta","event_id":"e-delta","index":0,"delta":{"type":"text","text":"héllo 🦀"}})
    }
    fn stop() -> Value {
        serde_json::json!({"event_type":"step.stop","event_id":"e-stop","index":0})
    }
    fn completed(full: bool) -> Value {
        let mut value = serde_json::json!({"event_type":"interaction.completed","event_id":"e-completed","interaction":{
            "id":"run-1","model":"gemini-pinned","status":"completed",
            "usage":{"total_input_tokens":20,"total_cached_tokens":2,"total_output_tokens":5,"total_thought_tokens":0,"total_tool_use_tokens":0}
        }});
        if full {
            value["interaction"]["steps"] = serde_json::json!([{"type":"model_output","content":[{"type":"text","text":"answer"}]}]);
        }
        value
    }
    fn frame(value: &Value, separator: &str) -> Vec<u8> {
        format!(
            "event: {}{separator}data: {}{separator}{separator}",
            value["event_type"].as_str().unwrap(),
            serde_json::to_string(value).unwrap()
        )
        .into_bytes()
    }
    fn wire(full: bool, separator: &str) -> Vec<u8> {
        [created(), start(), delta(), stop(), completed(full)]
            .iter()
            .flat_map(|event| frame(event, separator))
            .collect()
    }
    fn run(bytes: &[u8], mode: StreamMode) -> (Vec<StreamFrame>, StreamCompletion) {
        let mut stream =
            NativeStream::new(mode, (mode == StreamMode::Replay).then(|| "run-1".into())).unwrap();
        let frames = stream.push(bytes).unwrap();
        (frames, stream.finish())
    }
    fn unresolved(completion: &StreamCompletion, reason: StreamIssue) {
        assert_eq!(completion.profile, ProfileVerdict::Unresolved(reason));
        assert_eq!(completion.diagnostics.issue, Some(reason));
    }

    #[test]
    fn every_chunk_split_preserves_crlf_unicode_and_early_created_identity() {
        let original = wire(false, "\r\n");
        for split in 0..=original.len() {
            let mut stream = NativeStream::new(StreamMode::Fresh, None).unwrap();
            let mut frames = stream.push(&original[..split]).unwrap();
            frames.extend(stream.push(&original[split..]).unwrap());
            let mut completion = stream.finish();
            frames.append(&mut completion.frames);
            let replay: Vec<u8> = frames
                .iter()
                .flat_map(|frame| frame.wire.clone())
                .chain(completion.tail.iter().copied())
                .collect();
            assert_eq!(replay, original, "split {split}");
            assert_eq!(frames[0].created_id.as_deref(), Some("run-1"));
            assert!(frames[..4].iter().all(|frame| !frame.terminal));
            assert!(frames[4].terminal);
            assert_eq!(
                completion.profile,
                ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
            );
        }
    }

    #[test]
    fn native_service_tiers_stream_without_standard_fallback_or_midstream_switches() {
        for tier in ["flex", "priority"] {
            let mut created = created();
            created["interaction"]["service_tier"] = tier.into();
            let mut completed = completed(true);
            completed["interaction"]["service_tier"] = tier.into();
            let bytes: Vec<_> = [created.clone(), completed.clone()]
                .iter()
                .flat_map(|event| frame(event, "\n"))
                .collect();
            let (_, result) = run(&bytes, StreamMode::Fresh);
            assert!(matches!(result.profile, ProfileVerdict::Supported(_)));
            completed["interaction"]["service_tier"] = "standard".into();
            let bytes: Vec<_> = [created, completed]
                .iter()
                .flat_map(|event| frame(event, "\n"))
                .collect();
            let (_, result) = run(&bytes, StreamMode::Fresh);
            unresolved(&result, StreamIssue::UnsupportedUsage);
        }
    }

    #[test]
    fn single_byte_chunks_and_bare_cr_preserve_full_wire_at_eof() {
        let original = wire(false, "\r");
        let mut stream = NativeStream::new(StreamMode::Fresh, None).unwrap();
        let mut frames = Vec::new();
        for byte in &original {
            frames.extend(stream.push(&[*byte]).unwrap());
        }
        let mut completion = stream.finish();
        assert_eq!(
            completion.frames.len(),
            1,
            "final CR was waiting for an optional LF"
        );
        frames.append(&mut completion.frames);
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|frame| frame.wire)
                .collect::<Vec<_>>(),
            original
        );
        assert_eq!(
            completion.profile,
            ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
        );
    }

    #[test]
    fn multiline_data_and_large_numbers_keep_original_final_json_tokens() {
        let mut prefix = [created(), start(), delta(), stop()]
            .iter()
            .flat_map(|event| frame(event, "\n"))
            .collect::<Vec<_>>();
        let interaction = br#"{ "id":"run-1", "status":"completed", "usage":{"total_input_tokens":9007199254740993,"total_cached_tokens":0,"total_output_tokens":5,"total_thought_tokens":0,"total_tool_use_tokens":0} }"#;
        prefix.extend_from_slice(b"event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\ndata: \"interaction\":");
        prefix.extend_from_slice(interaction);
        prefix.extend_from_slice(b"}\n\n");
        let (frames, completion) = run(&prefix, StreamMode::Fresh);
        assert_eq!(
            completion.terminal.as_ref().unwrap().interaction,
            interaction
        );
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|frame| frame.wire)
                .collect::<Vec<_>>(),
            prefix
        );
        assert_eq!(
            completion.profile,
            ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
        );
    }

    #[test]
    fn replay_cannot_attest_missing_history_but_full_final_steps_can() {
        let (_, partial) = run(&frame(&completed(false), "\n"), StreamMode::Replay);
        unresolved(&partial, StreamIssue::ReplayHistoryMissing);
        assert!(partial.terminal.is_some());
        let mut replay = frame(&delta(), "\n");
        replay.extend(frame(&completed(true), "\n"));
        let (_, full) = run(&replay, StreamMode::Replay);
        assert_eq!(
            full.profile,
            ProfileVerdict::Supported(ProofKind::FullSnapshot)
        );
        let (_, fresh_missing) = run(&frame(&completed(false), "\n"), StreamMode::Fresh);
        unresolved(&fresh_missing, StreamIssue::IncompleteHistory);
    }

    #[test]
    fn duplicate_event_ids_and_changed_interaction_ids_never_produce_proof() {
        let mut duplicate = frame(&created(), "\n");
        duplicate.extend(frame(&created(), "\n"));
        duplicate.extend(frame(&completed(true), "\n"));
        let (frames, result) = run(&duplicate, StreamMode::Fresh);
        assert_eq!(frames[0].created_id.as_deref(), Some("run-1"));
        assert!(frames[1].created_id.is_none());
        unresolved(&result, StreamIssue::DuplicateEventId);
        let mut changed = completed(true);
        changed["interaction"]["id"] = "run-2".into();
        let mut bytes = frame(&created(), "\n");
        bytes.extend(frame(&changed, "\n"));
        let (_, result) = run(&bytes, StreamMode::Fresh);
        unresolved(&result, StreamIssue::IdentityMismatch);
        let mut stream = NativeStream::new(StreamMode::Fresh, Some("other".into())).unwrap();
        let frames = stream.push(&frame(&created(), "\n")).unwrap();
        assert!(frames[0].created_id.is_none());
        unresolved(&stream.finish(), StreamIssue::IdentityMismatch);
    }

    #[test]
    fn duplicate_json_keys_and_ambiguous_sse_metadata_are_unresolved() {
        for bytes in [
            br#"data: {"event_type":"interaction.created","interaction":{"id":"run-2","id":"run-1","status":"in_progress"}}

"#.as_slice(),
            b"event: interaction.created\nevent: interaction.completed\ndata: {}\n\n",
            b"id: a\ndata: {\"event_type\":\"interaction.created\",\"event_id\":\"b\",\"interaction\":{\"id\":\"run-1\",\"status\":\"in_progress\"}}\n\n",
        ] {
            let (frames,result)=run(bytes,StreamMode::Fresh);
            assert_eq!(frames.into_iter().flat_map(|frame|frame.wire).collect::<Vec<_>>(),bytes);
            unresolved(&result,StreamIssue::MalformedEvent);
        }
    }

    #[test]
    fn unsupported_tool_media_and_unknown_events_poison_even_full_final_snapshot() {
        for event in [
            serde_json::json!({"event_type":"step.start","index":0,"step":{"type":"code_execution_call","arguments":{}}}),
            serde_json::json!({"event_type":"step.delta","index":0,"delta":{"type":"image","data":"secret pixels"}}),
            serde_json::json!({"event_type":"provider.new_economic_feature","text":"secret"}),
        ] {
            let mut bytes = frame(&created(), "\n");
            bytes.extend(frame(&event, "\n"));
            bytes.extend(frame(&completed(true), "\n"));
            let (_, result) = run(&bytes, StreamMode::Fresh);
            unresolved(&result, StreamIssue::UnsupportedEvent);
            let diagnostic = serde_json::to_string(&result.diagnostics).unwrap();
            assert!(!diagnostic.contains("secret"));
            assert!(!diagnostic.contains("provider.new_economic_feature"));
        }
    }

    #[test]
    fn thought_summary_and_signature_events_are_supported_without_retaining_text() {
        let events = [
            created(),
            serde_json::json!({"event_type":"step.start","index":0,"step":{"type":"thought"}}),
            serde_json::json!({"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"private reasoning"}}}),
            serde_json::json!({"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"private signature"}}),
            serde_json::json!({"event_type":"step.stop","index":0}),
            completed(false),
        ];
        let bytes = events
            .iter()
            .flat_map(|event| frame(event, "\n"))
            .collect::<Vec<_>>();
        let (_, result) = run(&bytes, StreamMode::Fresh);
        assert_eq!(
            result.profile,
            ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
        );
        let diagnostic = serde_json::to_string(&result.diagnostics).unwrap();
        assert!(!diagnostic.contains("private"));
        assert!(diagnostic.len() < 4096);
        assert_eq!(
            result.diagnostics.delta_types,
            ["thought_summary".into(), "thought_signature".into()].into()
        );
    }

    #[test]
    fn fresh_stream_requires_closed_contiguous_steps_and_matching_delta_types() {
        for variant in 0..3 {
            let mut events = vec![created(), start(), delta()];
            match variant {
                0 => {}
                1 => {
                    events[1]["index"] = 1.into();
                    events[2]["index"] = 1.into();
                    let mut stop = stop();
                    stop["index"] = 1.into();
                    events.push(stop);
                }
                _ => {
                    events[2]["delta"] =
                        serde_json::json!({"type":"thought_signature","signature":"x"});
                    events.push(stop());
                }
            }
            events.push(completed(false));
            let bytes = events
                .iter()
                .flat_map(|event| frame(event, "\n"))
                .collect::<Vec<_>>();
            let (_, result) = run(&bytes, StreamMode::Fresh);
            unresolved(
                &result,
                if variant == 2 {
                    StreamIssue::InvalidSequence
                } else {
                    StreamIssue::IncompleteHistory
                },
            );
        }
    }

    #[test]
    fn status_update_only_and_missing_final_usage_never_fabricate_a_resource() {
        let status = serde_json::json!({"event_type":"interaction.status_update","interaction_id":"run-1","status":"completed","usage":{"total_input_tokens":20}});
        let mut bytes = frame(&created(), "\n");
        bytes.extend(frame(&status, "\n"));
        let (frames, result) = run(&bytes, StreamMode::Fresh);
        assert!(frames.last().unwrap().terminal);
        unresolved(&result, StreamIssue::MissingTerminal);
        assert!(result.terminal.is_none());
        let mut final_event = completed(false);
        final_event["interaction"]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        bytes.extend(frame(&final_event, "\n"));
        let (_, result) = run(&bytes, StreamMode::Fresh);
        unresolved(&result, StreamIssue::MissingUsage);
        assert!(result.terminal.is_some());
    }

    #[test]
    fn cut_frames_and_trailing_structured_events_prevent_eof_proof() {
        let mut bytes = wire(false, "\n");
        bytes.extend_from_slice(b"data: {\"event_type\":");
        let (_, cut) = run(&bytes, StreamMode::Fresh);
        unresolved(&cut, StreamIssue::CutFrame);
        assert_eq!(cut.tail, b"data: {\"event_type\":");
        let mut bytes = wire(false, "\n");
        let mut extra = delta();
        extra["event_id"] = "e-extra".into();
        bytes.extend(frame(&extra, "\n"));
        let (frames, result) = run(&bytes, StreamMode::Fresh);
        assert!(frames[4..].iter().all(|frame| frame.terminal));
        unresolved(&result, StreamIssue::InvalidSequence);
    }

    #[test]
    fn invalid_utf8_comments_and_bounds_never_lossily_change_wire_bytes() {
        let bytes = b"data: \xff\n\n";
        let (frames, result) = run(bytes, StreamMode::Fresh);
        assert_eq!(frames[0].wire, bytes);
        unresolved(&result, StreamIssue::InvalidUtf8);
        let mut bytes = b": heartbeat\r\n\r\n".to_vec();
        bytes.extend(wire(false, "\n"));
        let (_, result) = run(&bytes, StreamMode::Fresh);
        assert_eq!(
            result.profile,
            ProfileVerdict::Supported(ProofKind::ObservedFreshStream)
        );
        let mut stream = NativeStream::new(StreamMode::Fresh, None).unwrap();
        assert_eq!(
            stream.push(&vec![b'x'; MAX_FRAME_BYTES + 1]),
            Err(StreamIssue::SizeLimit)
        );
        unresolved(&stream.finish(), StreamIssue::SizeLimit);
        let mut stream = NativeStream::new(StreamMode::Fresh, None).unwrap();
        stream.push(&vec![b'x'; MAX_FRAME_BYTES]).unwrap();
        assert_eq!(stream.push(b"x"), Err(StreamIssue::SizeLimit));
        unresolved(&stream.finish(), StreamIssue::SizeLimit);
        assert!(NativeStream::new(StreamMode::Replay, None).is_err());
    }

    #[test]
    fn final_usage_tool_media_unknown_fields_and_model_changes_are_unresolved() {
        for variant in 0..4 {
            let mut final_event = completed(false);
            match variant {
                0 => final_event["interaction"]["usage"]["total_tool_use_tokens"] = 1.into(),
                1 => {
                    final_event["interaction"]["usage"]["output_tokens_by_modality"] =
                        serde_json::json!([{"modality":"image","tokens":5}])
                }
                2 => final_event["interaction"]["usage"]["new_billable_calls"] = 1.into(),
                _ => final_event["interaction"]["model"] = "changed".into(),
            }
            let mut bytes = [created(), start(), delta(), stop()]
                .iter()
                .flat_map(|event| frame(event, "\n"))
                .collect::<Vec<_>>();
            bytes.extend(frame(&final_event, "\n"));
            let (_, result) = run(&bytes, StreamMode::Fresh);
            unresolved(
                &result,
                if variant == 3 {
                    StreamIssue::ModelMismatch
                } else {
                    StreamIssue::UnsupportedUsage
                },
            );
        }
    }
}
