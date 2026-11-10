// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;

fn count(value: i128) -> Count {
    Count::from_integer(value).unwrap()
}

fn context() -> HostedContext {
    HostedContext {
        scope: [7; 32],
        evidence_ref: b"protected:hosted-fixture-and-tariff".to_vec(),
    }
}

fn provenance() -> Provenance {
    Provenance {
        account_namespace: "account-a".into(),
        tariff_version: "fixture-2026-11-10".into(),
        tool_version: "fixture-tool-v1".into(),
        stage: EvidenceStage::Final,
    }
}

fn config(tariff: HostedTariff) -> HostedConfig {
    let source = provenance();
    HostedConfig {
        account_namespace: source.account_namespace,
        tariff_version: source.tariff_version,
        tool_version: source.tool_version,
        tariff,
    }
}

fn rated(class: u32) -> RatedClass {
    RatedClass {
        class,
        rated_subject: 2,
        rate_available: true,
    }
}

fn container_config(variant: ContainerTariff) -> HostedConfig {
    config(HostedTariff::OpenAiContainer {
        variant,
        tiers: BTreeMap::from([("1g".into(), rated(10)), ("4g".into(), rated(11))]),
    })
}

fn container(billing: Option<ContainerBilling>) -> HostedEvidence {
    HostedEvidence::OpenAiContainer {
        provenance: provenance(),
        container_id: "cntr_same".into(),
        interval_id: Some("economic-interval-1".into()),
        memory_tier: Some("1g".into()),
        billing,
    }
}

fn observation(outcome: HostedOutcome) -> Observation {
    match outcome {
        HostedOutcome::Observation { observation, .. } => observation,
        other => panic!("expected final observation, got {other:?}"),
    }
}

fn unresolved(outcome: HostedOutcome, expected: UnresolvedReason) {
    assert!(
        matches!(outcome, HostedOutcome::Unresolved { reason, .. } if reason == expected),
        "{outcome:?}"
    );
}

fn stage(evidence: &mut HostedEvidence, stage: EvidenceStage) {
    match evidence {
        HostedEvidence::OpenAiContainer { provenance, .. }
        | HostedEvidence::AnthropicExecution { provenance, .. }
        | HostedEvidence::OpenAiImage { provenance, .. } => provenance.stage = stage,
    }
}

#[test]
fn container_identity_preserves_interval_reuse_and_separates_accounts() {
    let mut config = container_config(ContainerTariff::Session);
    let mut evidence = container(Some(ContainerBilling::Session));
    let first = observation(normalize(&context(), &config, &evidence));
    assert_eq!(
        first,
        observation(normalize(&context(), &config, &evidence))
    );
    assert_eq!(first.counts, BTreeMap::from([(10, Count::ONE)]));
    assert_eq!(first.scope, context().scope);
    assert_eq!(first.evidence_ref, context().evidence_ref);
    if let HostedEvidence::OpenAiContainer { interval_id, .. } = &mut evidence {
        *interval_id = Some("economic-interval-2".into());
    }
    let second = observation(normalize(&context(), &config, &evidence));
    assert_ne!(first.event_key, second.event_key);
    if let HostedEvidence::OpenAiContainer {
        provenance,
        interval_id,
        ..
    } = &mut evidence
    {
        *interval_id = Some("economic-interval-1".into());
        provenance.account_namespace = "account-b".into();
    }
    config.account_namespace = "account-b".into();
    let other_account = observation(normalize(&context(), &config, &evidence));
    assert_ne!(first.event_key, other_account.event_key);
    assert_eq!(
        Observation::decode(&first.encode().unwrap()).unwrap(),
        first
    );
}

#[test]
fn container_id_alone_is_not_an_interval_or_eligibility_proof() {
    let config = container_config(ContainerTariff::Session);
    let mut evidence = container(None);
    unresolved(
        normalize(&context(), &config, &evidence),
        UnresolvedReason::MissingEligibility,
    );
    if let HostedEvidence::OpenAiContainer { interval_id, .. } = &mut evidence {
        *interval_id = None;
    }
    unresolved(
        normalize(&context(), &config, &evidence),
        UnresolvedReason::MissingInterval,
    );
}

#[test]
fn container_tiers_and_minute_minimum_are_explicit_and_distinct() {
    let config = container_config(ContainerTariff::EligibleMinutes {
        duration: DurationRule {
            minimum_seconds: 300,
            quantum_seconds: 60,
        },
    });
    let mut evidence = container(Some(ContainerBilling::EligibleMinutes {
        duration_seconds: count(30),
    }));
    let small = observation(normalize(&context(), &config, &evidence));
    assert_eq!(small.counts, BTreeMap::from([(10, count(5))]));
    if let HostedEvidence::OpenAiContainer {
        memory_tier,
        billing,
        ..
    } = &mut evidence
    {
        *memory_tier = Some("4g".into());
        *billing = Some(ContainerBilling::EligibleMinutes {
            duration_seconds: count(301),
        });
    }
    let large = observation(normalize(&context(), &config, &evidence));
    assert_eq!(large.counts, BTreeMap::from([(11, count(6))]));
    // Conflicting tier evidence stays the same event; it cannot bypass host quarantine.
    assert_eq!(small.event_key, large.event_key);
    unresolved(
        normalize(
            &context(),
            &config,
            &container(Some(ContainerBilling::Session)),
        ),
        UnresolvedReason::MissingEligibility,
    );
}

#[test]
fn container_missing_rates_tiers_and_invalid_duration_are_unresolved() {
    let mut config = container_config(ContainerTariff::Session);
    if let HostedTariff::OpenAiContainer { tiers, .. } = &mut config.tariff {
        tiers.get_mut("1g").unwrap().rate_available = false;
    }
    unresolved(
        normalize(
            &context(),
            &config,
            &container(Some(ContainerBilling::Session)),
        ),
        UnresolvedReason::MissingRate,
    );
    let config = container_config(ContainerTariff::EligibleMinutes {
        duration: DurationRule {
            minimum_seconds: 300,
            quantum_seconds: 60,
        },
    });
    for seconds in [Count::from_micros(-1), Count::from_micros(i128::MAX)] {
        unresolved(
            normalize(
                &context(),
                &config,
                &container(Some(ContainerBilling::EligibleMinutes {
                    duration_seconds: seconds,
                })),
            ),
            UnresolvedReason::InvalidDuration,
        );
    }
    let mut evidence = container(Some(ContainerBilling::Session));
    if let HostedEvidence::OpenAiContainer { memory_tier, .. } = &mut evidence {
        *memory_tier = None;
    }
    unresolved(
        normalize(&context(), &config, &evidence),
        UnresolvedReason::MissingTier,
    );
}

fn anthropic_config(allowance: AllowancePolicy) -> HostedConfig {
    config(HostedTariff::AnthropicExecution {
        binding: rated(20),
        duration: DurationRule {
            minimum_seconds: 300,
            quantum_seconds: 1,
        },
        allowance,
    })
}

fn execution() -> HostedEvidence {
    HostedEvidence::AnthropicExecution {
        provenance: provenance(),
        container_id: "container-1".into(),
        interval_id: Some("execution-interval-1".into()),
        duration_seconds: Some(count(30)),
        disposition: ExecutionDisposition::Billable,
        code_execution_requests: Some(1),
        allocated_free_seconds: None,
    }
}

#[test]
fn anthropic_request_counter_never_substitutes_for_execution_duration() {
    let config = anthropic_config(AllowancePolicy::GrossDuration);
    let mut evidence = execution();
    if let HostedEvidence::AnthropicExecution {
        duration_seconds, ..
    } = &mut evidence
    {
        *duration_seconds = None;
    }
    let result = normalize(&context(), &config, &evidence);
    unresolved(result.clone(), UnresolvedReason::InvalidDuration);
    assert!(matches!(
        result,
        HostedOutcome::Unresolved {
            diagnostics: HostedDiagnostics {
                code_execution_requests: Some(1),
                ..
            },
            ..
        }
    ));
}

#[test]
fn anthropic_preloading_can_bill_with_zero_requests_and_exemption_is_separate() {
    let config = anthropic_config(AllowancePolicy::GrossDuration);
    let mut evidence = execution();
    if let HostedEvidence::AnthropicExecution {
        code_execution_requests,
        ..
    } = &mut evidence
    {
        *code_execution_requests = Some(0);
    }
    let outcome = normalize(&context(), &config, &evidence);
    assert_eq!(
        observation(outcome.clone()).counts,
        BTreeMap::from([(20, count(300))])
    );
    assert!(matches!(
        outcome,
        HostedOutcome::Observation {
            diagnostics: HostedDiagnostics {
                code_execution_requests: Some(0),
                gross_duration: true,
                ..
            },
            ..
        }
    ));
    if let HostedEvidence::AnthropicExecution { disposition, .. } = &mut evidence {
        *disposition = ExecutionDisposition::Exempt;
    }
    let exempt = normalize(
        &context(),
        &anthropic_config(AllowancePolicy::Unresolved),
        &evidence,
    );
    assert_eq!(
        observation(exempt.clone()).counts,
        BTreeMap::from([(20, Count::ZERO)])
    );
    assert!(
        matches!(exempt, HostedOutcome::Observation { diagnostics: HostedDiagnostics { exempt_seconds: Some(seconds), .. }, .. } if seconds == count(30))
    );
}

#[test]
fn anthropic_account_allowance_requires_explicit_policy_and_valid_allocation() {
    let mut evidence = execution();
    unresolved(
        normalize(
            &context(),
            &anthropic_config(AllowancePolicy::Unresolved),
            &evidence,
        ),
        UnresolvedReason::MissingAllowance,
    );
    let config = anthropic_config(AllowancePolicy::Reconciled);
    unresolved(
        normalize(&context(), &config, &evidence),
        UnresolvedReason::MissingAllowance,
    );
    for free in [count(-1), count(301)] {
        if let HostedEvidence::AnthropicExecution {
            allocated_free_seconds,
            ..
        } = &mut evidence
        {
            *allocated_free_seconds = Some(free);
        }
        unresolved(
            normalize(&context(), &config, &evidence),
            UnresolvedReason::MissingAllowance,
        );
    }
    if let HostedEvidence::AnthropicExecution {
        allocated_free_seconds,
        ..
    } = &mut evidence
    {
        *allocated_free_seconds = Some(count(120));
    }
    assert_eq!(
        observation(normalize(&context(), &config, &evidence)).counts,
        BTreeMap::from([(20, count(180))])
    );
}

fn image_binding(subject: u32) -> ImageBinding {
    ImageBinding {
        rated_subject: subject,
        text_input: 30,
        image_input: 31,
        cached_text_input: 32,
        cached_image_input: 33,
        text_output: 34,
        image_output: 35,
        priced_classes: (30..=35).collect(),
    }
}

fn image_config() -> HostedConfig {
    config(HostedTariff::OpenAiImage {
        models: BTreeMap::from([
            ("image-a".into(), image_binding(3)),
            ("image-b".into(), image_binding(4)),
        ]),
        documented_default: Some(ImageDefault {
            parent_model: "parent-pinned".into(),
            leaf_model: "image-a".into(),
            documentation_ref: "protected:documented-default-fixture".into(),
        }),
    })
}

fn image() -> HostedEvidence {
    HostedEvidence::OpenAiImage {
        provenance: provenance(),
        parent_id: "resp-original".into(),
        tool_call_id: "ig-1".into(),
        parent_model: "parent-pinned".into(),
        reported_model: Some("image-b".into()),
        requested_model: Some("image-a".into()),
        usage: Some(ImageUsage {
            schema: "inclusive_modality_tokens_v1".into(),
            input_tokens: Some(150),
            text_input_tokens: Some(100),
            image_input_tokens: Some(50),
            cached_text_input_tokens: Some(10),
            cached_image_input_tokens: Some(20),
            output_tokens: Some(200),
            text_output_tokens: Some(0),
            image_output_tokens: Some(200),
        }),
    }
}

#[test]
fn image_leaf_counts_are_nonoverlapping_and_never_parent_rated() {
    let result = observation(normalize(&context(), &image_config(), &image()));
    assert_eq!(result.rated_subject, 4);
    assert_eq!(
        result.counts,
        BTreeMap::from([
            (30, count(90)),
            (31, count(30)),
            (32, count(10)),
            (33, count(20)),
            (34, count(0)),
            (35, count(200))
        ])
    );
    let total = result
        .counts
        .values()
        .try_fold(Count::ZERO, |sum, value| sum.checked_add(*value))
        .unwrap();
    assert_eq!(total, count(350));
    assert_eq!(result.component_key, br#"["image_generation","ig-1"]"#);
}

#[test]
fn image_resolution_is_reported_then_request_then_pinned_default() {
    let config = image_config();
    let mut evidence = image();
    assert_eq!(
        observation(normalize(&context(), &config, &evidence)).rated_subject,
        4
    );
    if let HostedEvidence::OpenAiImage { reported_model, .. } = &mut evidence {
        *reported_model = None;
    }
    assert_eq!(
        observation(normalize(&context(), &config, &evidence)).rated_subject,
        3
    );
    if let HostedEvidence::OpenAiImage {
        requested_model, ..
    } = &mut evidence
    {
        *requested_model = None;
    }
    assert_eq!(
        observation(normalize(&context(), &config, &evidence)).rated_subject,
        3
    );
    if let HostedEvidence::OpenAiImage { parent_model, .. } = &mut evidence {
        *parent_model = "different-parent".into();
    }
    unresolved(
        normalize(&context(), &config, &evidence),
        UnresolvedReason::MissingModel,
    );
}

#[test]
fn unknown_or_empty_reported_model_does_not_fall_back_to_request() {
    let mut evidence = image();
    if let HostedEvidence::OpenAiImage { reported_model, .. } = &mut evidence {
        *reported_model = Some("unknown-image".into());
    }
    unresolved(
        normalize(&context(), &image_config(), &evidence),
        UnresolvedReason::MissingBinding,
    );
    if let HostedEvidence::OpenAiImage { reported_model, .. } = &mut evidence {
        *reported_model = Some(String::new());
    }
    unresolved(
        normalize(&context(), &image_config(), &evidence),
        UnresolvedReason::MissingModel,
    );
}

#[test]
fn absent_cache_unknown_schema_and_inconsistent_image_totals_stay_unresolved() {
    for change in 0..5 {
        let mut evidence = image();
        if let HostedEvidence::OpenAiImage {
            usage: Some(usage), ..
        } = &mut evidence
        {
            match change {
                0 => usage.cached_image_input_tokens = None,
                1 => usage.schema = "unrecognized".into(),
                2 => usage.cached_text_input_tokens = Some(101),
                3 => usage.input_tokens = Some(151),
                _ => usage.image_output_tokens = Some(201),
            }
        }
        unresolved(
            normalize(&context(), &image_config(), &evidence),
            UnresolvedReason::InvalidImageUsage,
        );
    }
}

#[test]
fn image_binding_rate_and_class_collisions_fail_closed() {
    for (change, expected) in [
        (0, UnresolvedReason::MissingBinding),
        (1, UnresolvedReason::MissingRate),
        (2, UnresolvedReason::OverlappingClasses),
    ] {
        let mut config = image_config();
        if let HostedTariff::OpenAiImage { models, .. } = &mut config.tariff {
            let binding = models.get_mut("image-b").unwrap();
            match change {
                0 => binding.rated_subject = 0,
                1 => {
                    binding.priced_classes.remove(&35);
                }
                _ => binding.cached_text_input = binding.text_input,
            }
        }
        unresolved(normalize(&context(), &config, &image()), expected);
    }
}

#[test]
fn historical_partial_and_pending_never_create_final_zero_observations() {
    for (mut evidence, config) in [
        (image(), image_config()),
        (
            execution(),
            anthropic_config(AllowancePolicy::GrossDuration),
        ),
        (
            container(Some(ContainerBilling::Session)),
            container_config(ContainerTariff::Session),
        ),
    ] {
        stage(&mut evidence, EvidenceStage::Historical);
        assert!(matches!(
            normalize(&context(), &config, &evidence),
            HostedOutcome::Ignored {
                reason: IgnoredReason::Historical,
                ..
            }
        ));
        stage(&mut evidence, EvidenceStage::Partial);
        assert!(matches!(
            normalize(&context(), &config, &evidence),
            HostedOutcome::Ignored {
                reason: IgnoredReason::Partial,
                ..
            }
        ));
        stage(&mut evidence, EvidenceStage::Pending);
        unresolved(
            normalize(&context(), &config, &evidence),
            UnresolvedReason::Pending,
        );
    }
}

#[test]
fn custody_provenance_and_identity_bounds_are_required() {
    let config = image_config();
    let evidence = image();
    let mut missing = context();
    missing.scope = [0; 32];
    unresolved(
        normalize(&missing, &config, &evidence),
        UnresolvedReason::MissingCustody,
    );
    missing = context();
    missing.evidence_ref.clear();
    unresolved(
        normalize(&missing, &config, &evidence),
        UnresolvedReason::MissingCustody,
    );
    let mut mismatch = config.clone();
    mismatch.tariff_version = "new-tariff-cannot-reprice-existing-event".into();
    unresolved(
        normalize(&context(), &mismatch, &evidence),
        UnresolvedReason::ProvenanceMismatch,
    );
    let mut long = evidence.clone();
    if let HostedEvidence::OpenAiImage { parent_id, .. } = &mut long {
        *parent_id = "x".repeat(257);
    }
    unresolved(
        normalize(&context(), &config, &long),
        UnresolvedReason::InvalidObservation,
    );
    assert_ne!(key(&["a:b", "c"]).unwrap(), key(&["a", "b:c"]).unwrap());
}

#[test]
fn typed_evidence_roundtrips_and_unknown_fields_are_refused() {
    let evidence = image();
    let mut value = serde_json::to_value(&evidence).unwrap();
    let decoded: HostedEvidence = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        normalize(&context(), &image_config(), &evidence),
        normalize(&context(), &image_config(), &decoded)
    );
    value["invented_billable_sessions"] = serde_json::json!(2);
    assert!(serde_json::from_value::<HostedEvidence>(value).is_err());
}

#[test]
fn duration_rule_cannot_silently_round_an_unrepresentable_minute_quantity() {
    let config = container_config(ContainerTariff::EligibleMinutes {
        duration: DurationRule {
            minimum_seconds: 300,
            quantum_seconds: 1,
        },
    });
    unresolved(
        normalize(
            &context(),
            &config,
            &container(Some(ContainerBilling::EligibleMinutes {
                duration_seconds: count(301),
            })),
        ),
        UnresolvedReason::InvalidDuration,
    );
    let config = container_config(ContainerTariff::EligibleMinutes {
        duration: DurationRule {
            minimum_seconds: 300,
            quantum_seconds: 0,
        },
    });
    unresolved(
        normalize(
            &context(),
            &config,
            &container(Some(ContainerBilling::EligibleMinutes {
                duration_seconds: count(300),
            })),
        ),
        UnresolvedReason::InvalidDurationRule,
    );
}
