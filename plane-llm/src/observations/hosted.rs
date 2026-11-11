// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Nonshipping reference normalization of authenticated hosted-tool evidence.
//!
//! This is a typed evidence boundary, not a decoder for arbitrary provider JSON.
//! Its caller must authenticate evidence and obtain configuration/scope from the
//! host. In particular, a client cannot assert its own authenticated provenance or scope.
//! Raw evidence and the measurement schema belong in protected storage behind
//! `evidence_ref`. Outputs must never also become ordinary billable UnitCounts.
//! Compiled only for reference tests. It does not wire native provider routes,
//! durable provider-ID lookup/checkpoints, scope recovery or reconciliation jobs.
//!
//! Provider documentation checked 2026-11-10:
//! - <https://developers.openai.com/api/docs/pricing>
//! - <https://developers.openai.com/api/docs/guides/tools-image-generation>
//! - <https://developers.openai.com/api/docs/guides/image-generation>
//! - <https://platform.claude.com/docs/en/agents-and-tools/tool-use/code-execution-tool>
//!
//! Container session and measured-duration facts require authoritative interval
//! evidence. Neither follows from a container ID/idle timeout.
//! Anthropic's request count does not measure duration; file preloading can cost
//! execution time without a tool call. Image cache counts can be absent from the
//! Responses API: absent never means zero here.

use busbar_contract::{
    abi::sdk::plane::observation::{Observation, ScopeId},
    Count,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Host custody and protected evidence location; never sourced from client JSON.
#[derive(Debug, Clone)]
pub struct HostedContext {
    /// Host-authorized original accounting scope.
    pub scope: ScopeId,
    /// Reference to raw evidence and its schema, not the evidence itself.
    pub evidence_ref: Vec<u8>,
}

/// Class bindings and measurement schema for one account and tool version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedConfig {
    /// Stable provider-account namespace, never an API credential.
    pub account_namespace: String,
    /// Pinned measurement schema identity.
    pub schema_version: String,
    /// Exact tool/API version covered by this configuration.
    pub tool_version: String,
    /// Provider-specific physical quantities; prices and invoice treatment stay with the host.
    pub measurement: HostedMeasurement,
}

/// A declared class and host-approved subject binding, without monetary policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassBinding {
    /// Declared billable class.
    pub class: u32,
    /// Index into the host-approved accounting subjects.
    pub rated_subject: u32,
}

/// Physical container quantity to extract from authenticated evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerMeasure {
    /// One observed session identified by authoritative interval evidence.
    Session,
    /// Measured seconds, with no minimum or rounding.
    DurationSeconds,
}

/// The six nonoverlapping image-token classes, all rated to one actual leaf model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBinding {
    /// Host-approved leaf subject; zero (the parent serving subject) is forbidden.
    pub rated_subject: u32,
    /// Uncached text input class.
    pub text_input: u32,
    /// Uncached image input class.
    pub image_input: u32,
    /// Cached text input class.
    pub cached_text_input: u32,
    /// Cached image input class.
    pub cached_image_input: u32,
    /// Text output class, with explicit zero usage when the schema excludes it.
    pub text_output: u32,
    /// Image output class.
    pub image_output: u32,
}

/// A documented leaf default valid for one exact parent model and tool version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageDefault {
    /// Exact parent model; aliases/defaults must be pinned by the operator.
    pub parent_model: String,
    /// Leaf model identity.
    pub leaf_model: String,
    /// Protected reference to the dated documentation establishing this default.
    pub documentation_ref: String,
}

/// Provider-owned measurements; no prices, duration rounding, or allowance allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostedMeasurement {
    /// OpenAI hosted shell/code interpreter containers.
    OpenAiContainer {
        /// Physical quantity selected for this component.
        variant: ContainerMeasure,
        /// Exact memory tiers mapped to declared classes.
        tiers: BTreeMap<String, ClassBinding>,
    },
    /// Anthropic measured execution seconds.
    AnthropicExecution {
        /// Seconds class and host subject.
        binding: ClassBinding,
    },
    /// Responses image-generation leaf usage, independent of parent LLM usage.
    OpenAiImage {
        /// Exact image model to approved accounting binding.
        models: BTreeMap<String, ImageBinding>,
        /// Optional documented default; never a hardcoded latest model.
        documented_default: Option<ImageDefault>,
    },
}

/// Whether evidence is a current authoritative total or a replay/partial item.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStage {
    /// Complete authoritative usage, including measured failed/cancelled work.
    Final,
    /// Usage is not yet final; do not release the host's reservation.
    Pending,
    /// A historical input/reference, not newly measured work.
    Historical,
    /// A partial-image or other intermediate streaming event.
    Partial,
}

/// Common authenticated provenance for hosted-tool facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Provider-account namespace authenticated by the caller.
    pub account_namespace: String,
    /// Original measurement schema for this event.
    pub schema_version: String,
    /// Tool/API version recorded with original evidence.
    pub tool_version: String,
    /// Completeness classification, not inferred from a tool result's presence.
    pub stage: EvidenceStage,
}

/// Explicit measured container usage, independent of a container identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerUsage {
    /// Provider evidence identifies exactly one session interval.
    Session,
    /// Provider evidence supplies a measured interval duration.
    DurationSeconds {
        /// Authoritative total seconds, not request latency or idle timeout.
        duration_seconds: Count,
    },
}

/// Complete image usage in one deliberately narrow, inclusive-input schema.
///
/// Input modality totals include their cache subsets. Output totals contain all
/// reported output, including any partial-image overhead, exactly once. The caller
/// must establish this schema from authenticated evidence; plain Responses usage
/// with missing cache detail is insufficient.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUsage {
    /// Must equal `inclusive_modality_tokens_v1`; unrecognized schemas stay unresolved.
    pub schema: String,
    /// Aggregate leaf input total, used only to validate modality coverage.
    pub input_tokens: Option<u64>,
    /// Inclusive text input total.
    pub text_input_tokens: Option<u64>,
    /// Inclusive image input total.
    pub image_input_tokens: Option<u64>,
    /// Cached subset of text input; None is unknown, not zero.
    pub cached_text_input_tokens: Option<u64>,
    /// Cached subset of image input; None is unknown, not zero.
    pub cached_image_input_tokens: Option<u64>,
    /// Aggregate leaf output total, used only to validate modality coverage.
    pub output_tokens: Option<u64>,
    /// Text output tokens; explicit zero is required when absent by schema.
    pub text_output_tokens: Option<u64>,
    /// Image output tokens.
    pub image_output_tokens: Option<u64>,
}

/// Typed facts extracted by an authenticated provider evidence adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostedEvidence {
    /// OpenAI container facts. An ID alone never proves one observed session.
    OpenAiContainer {
        /// Original measurement provenance.
        provenance: Provenance,
        /// Upstream container ID, namespaced by account.
        container_id: String,
        /// Authoritative economic interval; independent of idle timeout.
        interval_id: Option<String>,
        /// Exact provider memory tier.
        memory_tier: Option<String>,
        /// Explicit measured session or duration evidence.
        usage: Option<ContainerUsage>,
    },
    /// Anthropic execution interval facts; request counts remain diagnostic.
    AnthropicExecution {
        /// Original measurement provenance.
        provenance: Provenance,
        /// Upstream execution container.
        container_id: String,
        /// Authoritative observed execution interval.
        interval_id: Option<String>,
        /// Authoritative interval duration, including preloading when applicable.
        duration_seconds: Option<Count>,
        /// Original code_execution_requests, never interpreted as sessions.
        code_execution_requests: Option<u64>,
    },
    /// OpenAI Responses image-generation leaf facts.
    OpenAiImage {
        /// Original measurement provenance.
        provenance: Provenance,
        /// Original parent response/run ID; never the retrieval request ID.
        parent_id: String,
        /// Stable image-generation tool-call ID.
        tool_call_id: String,
        /// Actual parent model, needed only for a documented default.
        parent_model: String,
        /// Explicitly reported actual leaf model has first priority.
        reported_model: Option<String>,
        /// Original request's tool model has second priority.
        requested_model: Option<String>,
        /// Authoritative complete leaf usage; never parent aggregate usage.
        usage: Option<ImageUsage>,
    },
}

/// Nonbilling diagnostics kept apart from billable classes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostedDiagnostics {
    /// Provider request counter, which is not a container-session count.
    pub code_execution_requests: Option<u64>,
    /// Selected image model, useful for explaining the rated subject.
    pub leaf_model: Option<String>,
}

/// Why authoritative settlement cannot yet be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// Missing host scope or protected raw-evidence reference.
    MissingCustody,
    /// Configuration and authenticated evidence do not name the same provenance.
    ProvenanceMismatch,
    /// Evidence is not final.
    Pending,
    /// Container or execution interval is absent/empty.
    MissingInterval,
    /// Container memory tier is absent or has no configured binding.
    MissingTier,
    /// Requested physical quantity is absent or mismatched.
    MissingMeasurement,
    /// Duration is absent, negative, inexact or outside supported arithmetic.
    InvalidDuration,
    /// No actual leaf model can be established.
    MissingModel,
    /// Actual leaf model lacks an approved accounting subject.
    MissingBinding,
    /// Duplicate classes would overlap or overwrite leaf quantities.
    OverlappingClasses,
    /// Image schema is unknown, incomplete or internally inconsistent.
    InvalidImageUsage,
    /// Evidence and configured provider component differ.
    MeasurementMismatch,
    /// Identity or SDK observation bounds were exceeded.
    InvalidObservation,
}

/// Nonbilling replay/intermediate outcomes; these never settle a zero final total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoredReason {
    /// Original tool item supplied as historical context.
    Historical,
    /// Intermediate streaming item; eventual final totals own the charge.
    Partial,
}

/// A final owned SDK observation or an explicit nonsettlement outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedOutcome {
    /// Complete authoritative totals; duplicate admission belongs to the host ledger.
    Observation {
        /// Original event/component identity and nonoverlapping counts.
        observation: Observation,
        /// Provider diagnostics that must not be billed as ordinary counts.
        diagnostics: HostedDiagnostics,
    },
    /// Keep the host reservation and protected evidence for reconciliation.
    Unresolved {
        /// Precise missing or conflicting evidence.
        reason: UnresolvedReason,
        /// Diagnostics retained even when the requested measurement is unknown.
        diagnostics: HostedDiagnostics,
    },
    /// No settlement: historical input or intermediate output.
    Ignored {
        /// The item's nonbilling role.
        reason: IgnoredReason,
        /// Any diagnostic information retained separately.
        diagnostics: HostedDiagnostics,
    },
}

/// Normalize one provider economic component without performing I/O or pricing.
///
/// Repeated final evidence produces the same event/component keys. Host durable
/// admission, not process-local state, prevents charging it again. This function
/// never infers event order from increasing totals and never creates corrections.
pub fn normalize(
    context: &HostedContext,
    config: &HostedConfig,
    evidence: &HostedEvidence,
) -> HostedOutcome {
    let mut diagnostics = HostedDiagnostics::default();
    let provenance = match evidence {
        HostedEvidence::OpenAiContainer { provenance, .. }
        | HostedEvidence::OpenAiImage { provenance, .. } => provenance,
        HostedEvidence::AnthropicExecution {
            provenance,
            code_execution_requests,
            ..
        } => {
            diagnostics.code_execution_requests = *code_execution_requests;
            provenance
        }
    };
    let ignored = match provenance.stage {
        EvidenceStage::Historical => Some(IgnoredReason::Historical),
        EvidenceStage::Partial => Some(IgnoredReason::Partial),
        _ => None,
    };
    if let Some(reason) = ignored {
        return HostedOutcome::Ignored {
            reason,
            diagnostics,
        };
    }
    let result = (|| {
        if context.scope == [0; 32]
            || context.evidence_ref.is_empty()
            || context.evidence_ref.len() > 1024
        {
            return Err(UnresolvedReason::MissingCustody);
        }
        if config.account_namespace.is_empty()
            || config.schema_version.is_empty()
            || config.tool_version.is_empty()
            || config.account_namespace != provenance.account_namespace
            || config.schema_version != provenance.schema_version
            || config.tool_version != provenance.tool_version
        {
            return Err(UnresolvedReason::ProvenanceMismatch);
        }
        if matches!(provenance.stage, EvidenceStage::Pending) {
            return Err(UnresolvedReason::Pending);
        }
        normalize_final(context, config, evidence, &mut diagnostics)
    })();
    match result {
        Ok(observation) => HostedOutcome::Observation {
            observation,
            diagnostics,
        },
        Err(reason) => HostedOutcome::Unresolved {
            reason,
            diagnostics,
        },
    }
}

fn normalize_final(
    context: &HostedContext,
    config: &HostedConfig,
    evidence: &HostedEvidence,
    diagnostics: &mut HostedDiagnostics,
) -> Result<Observation, UnresolvedReason> {
    let (event, component, subject, counts) = match (&config.measurement, evidence) {
        (
            HostedMeasurement::OpenAiContainer { variant, tiers },
            HostedEvidence::OpenAiContainer {
                container_id,
                interval_id,
                memory_tier,
                usage,
                ..
            },
        ) => {
            let interval = interval(interval_id)?;
            let binding = memory_tier
                .as_ref()
                .and_then(|tier| tiers.get(tier))
                .ok_or(UnresolvedReason::MissingTier)?;
            let amount = match (variant, usage) {
                (ContainerMeasure::Session, Some(ContainerUsage::Session)) => Count::ONE,
                (
                    ContainerMeasure::DurationSeconds,
                    Some(ContainerUsage::DurationSeconds { duration_seconds }),
                ) => measured_seconds(*duration_seconds)?,
                _ => return Err(UnresolvedReason::MissingMeasurement),
            };
            (
                key(&["openai", &config.account_namespace, container_id, interval])?,
                b"container".to_vec(),
                binding.rated_subject,
                vec![(binding.class, amount)],
            )
        }
        (
            HostedMeasurement::AnthropicExecution { binding },
            HostedEvidence::AnthropicExecution {
                container_id,
                interval_id,
                duration_seconds,
                ..
            },
        ) => {
            let interval = interval(interval_id)?;
            let amount =
                measured_seconds(duration_seconds.ok_or(UnresolvedReason::InvalidDuration)?)?;
            (
                key(&[
                    "anthropic",
                    &config.account_namespace,
                    container_id,
                    interval,
                ])?,
                b"execution".to_vec(),
                binding.rated_subject,
                vec![(binding.class, amount)],
            )
        }
        (
            HostedMeasurement::OpenAiImage {
                models,
                documented_default,
            },
            HostedEvidence::OpenAiImage {
                parent_id,
                tool_call_id,
                parent_model,
                reported_model,
                requested_model,
                usage,
                ..
            },
        ) => {
            // A present but empty/unknown explicit model cannot fall back to a cheaper model.
            let model = reported_model
                .as_deref()
                .or(requested_model.as_deref())
                .or_else(|| {
                    documented_default
                        .as_ref()
                        .filter(|default| {
                            default.parent_model == *parent_model
                                && !default.documentation_ref.is_empty()
                        })
                        .map(|default| default.leaf_model.as_str())
                })
                .filter(|model| !model.is_empty())
                .ok_or(UnresolvedReason::MissingModel)?;
            diagnostics.leaf_model = Some(model.to_owned());
            let binding = models.get(model).ok_or(UnresolvedReason::MissingBinding)?;
            if binding.rated_subject == 0 {
                return Err(UnresolvedReason::MissingBinding);
            }
            let classes = [
                binding.text_input,
                binding.image_input,
                binding.cached_text_input,
                binding.cached_image_input,
                binding.text_output,
                binding.image_output,
            ];
            if classes.iter().copied().collect::<BTreeSet<_>>().len() != classes.len() {
                return Err(UnresolvedReason::OverlappingClasses);
            }
            let quantities =
                image_quantities(usage.as_ref().ok_or(UnresolvedReason::InvalidImageUsage)?)?;
            let counts = classes
                .into_iter()
                .zip(quantities)
                .map(|(class, amount)| (class, Count::from_micros(i128::from(amount) * 1_000_000)))
                .collect();
            (
                key(&["openai", &config.account_namespace, parent_id])?,
                key(&["image_generation", tool_call_id])?,
                binding.rated_subject,
                counts,
            )
        }
        _ => return Err(UnresolvedReason::MeasurementMismatch),
    };
    let mut observation = Observation::new(context.scope, event, component)
        .rated_to(subject)
        .evidence(context.evidence_ref.clone())
        .final_totals();
    for (class, amount) in counts {
        if !amount.fits_a_64_bit_column() {
            return Err(UnresolvedReason::InvalidObservation);
        }
        observation = observation
            .count(class, amount)
            .map_err(|_| UnresolvedReason::InvalidObservation)?;
    }
    observation
        .encode()
        .map_err(|_| UnresolvedReason::InvalidObservation)?;
    Ok(observation)
}

fn interval(value: &Option<String>) -> Result<&str, UnresolvedReason> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(UnresolvedReason::MissingInterval)
}

fn key(parts: &[&str]) -> Result<Vec<u8>, UnresolvedReason> {
    if parts.iter().any(|part| part.is_empty() || part.len() > 256) {
        return Err(UnresolvedReason::InvalidObservation);
    }
    // Structured encoding avoids delimiter ambiguity without adding a hashing dependency.
    let key = serde_json::to_vec(parts).map_err(|_| UnresolvedReason::InvalidObservation)?;
    if key.len() > 256 {
        return Err(UnresolvedReason::InvalidObservation);
    }
    Ok(key)
}

fn measured_seconds(seconds: Count) -> Result<Count, UnresolvedReason> {
    if seconds < Count::ZERO || !seconds.fits_a_64_bit_column() {
        return Err(UnresolvedReason::InvalidDuration);
    }
    Ok(seconds)
}

fn image_quantities(usage: &ImageUsage) -> Result<[u64; 6], UnresolvedReason> {
    let invalid = UnresolvedReason::InvalidImageUsage;
    if usage.schema != "inclusive_modality_tokens_v1" {
        return Err(invalid);
    }
    let [input, text, image, cached_text, cached_image, output, text_output, image_output] = [
        usage.input_tokens,
        usage.text_input_tokens,
        usage.image_input_tokens,
        usage.cached_text_input_tokens,
        usage.cached_image_input_tokens,
        usage.output_tokens,
        usage.text_output_tokens,
        usage.image_output_tokens,
    ]
    .map(|value| value.ok_or(invalid));
    let (input, text, image, cached_text, cached_image, output, text_output, image_output) = (
        input?,
        text?,
        image?,
        cached_text?,
        cached_image?,
        output?,
        text_output?,
        image_output?,
    );
    if text.checked_add(image) != Some(input)
        || text_output.checked_add(image_output) != Some(output)
    {
        return Err(invalid);
    }
    Ok([
        text.checked_sub(cached_text).ok_or(invalid)?,
        image.checked_sub(cached_image).ok_or(invalid)?,
        cached_text,
        cached_image,
        text_output,
        image_output,
    ])
}

#[cfg(test)]
#[path = "hosted_tests.rs"]
mod tests;
