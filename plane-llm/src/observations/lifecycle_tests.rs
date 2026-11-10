use super::*;
use busbar_contract::abi::sdk::plane::observation::Receipt;

fn partition() -> Partition {
    Partition::new(&"a".repeat(64), "account-a").unwrap()
}
fn binding() -> Binding {
    Binding {
        scope: [7; 32],
        account_namespace: "account-a".into(),
        route_entry: "original-member".into(),
        model: "model-pinned".into(),
        pricing_mode: "standard".into(),
        usage_schema: "gemini-interactions-2026-11-10".into(),
        input_class: 0,
        cache_read_class: 2,
        output_class: 1,
        tool_use_class: None,
        priced_classes: vec![0, 1, 2],
    }
}
fn applied(correlation: u64) -> ConditionalResult {
    ConditionalResult {
        correlation,
        decision: Ok(Decision::Applied),
    }
}
fn new_run() -> Run {
    prepare_create(0, 1, partition(), "server-intent-a", binding())
        .unwrap()
        .committed(&applied(1))
        .unwrap()
        .into_run()
}
fn bound() -> Run {
    let r = new_run();
    r.bind_provider(0, 2, &r.encode().unwrap(), "provider-a")
        .unwrap()
        .committed(&applied(2))
        .unwrap()
}
fn observation(r: &Run) -> Observation {
    Observation::new(
        r.binding.scope,
        serde_json::to_vec(&("account-a", "provider-a")).unwrap(),
        b"output".to_vec(),
    )
    .count(1, busbar_contract::Count::from(4))
    .unwrap()
    .final_totals()
}

#[test]
fn dispatch_requires_exact_applied_create_receipt() {
    for result in [
        ConditionalResult {
            correlation: 1,
            decision: Ok(Decision::ConditionConflict),
        },
        applied(2),
        ConditionalResult {
            correlation: 1,
            decision: Err(
                busbar_contract::abi::sdk::plane::conditional::ConditionalFailure::Indeterminate,
            ),
        },
    ] {
        assert!(prepare_create(0, 1, partition(), "intent", binding())
            .unwrap()
            .committed(&result)
            .is_err());
    }
}
#[test]
fn recovered_lost_create_never_grants_a_second_send() {
    let r = new_run();
    let restored = Run::decode(&r.encode().unwrap(), &partition()).unwrap();
    assert_eq!(restored.recovery(), Recovery::LostCreate);
}
#[test]
fn id_binding_is_atomic_absent_only_with_original_record_cas() {
    let r = new_run();
    let c = r
        .bind_provider(0, 2, &r.encode().unwrap(), "provider-a")
        .unwrap();
    assert_eq!(c.candidate.predicates.len(), 2);
    assert_eq!(c.candidate.mutations.len(), 2);
    assert!(matches!(
        c.candidate.predicates[1].expected,
        Expected::Absent
    ));
    let r = c.committed(&applied(2)).unwrap();
    assert!(r
        .bind_provider(0, 3, &r.encode().unwrap(), "provider-b")
        .is_err());
    assert_eq!(
        r.recovery(),
        Recovery::Poll {
            id: "provider-a".into(),
            attach: ScopeRequest::attach([7; 32])
        }
    );
}
#[test]
fn principal_account_and_configuration_are_pinned_on_restart() {
    let r = bound();
    let bytes = r.encode().unwrap();
    let other = Partition::new(&"b".repeat(64), "account-a").unwrap();
    assert!(Run::decode(&bytes, &other).is_err());
    let other = Partition::new(&"a".repeat(64), "account-b").unwrap();
    assert!(Run::decode(&bytes, &other).is_err());
    let restored = Run::decode(&bytes, &partition()).unwrap();
    assert_eq!(restored.binding().model, "model-pinned");
    assert_eq!(restored.binding().scope, [7; 32]);
    assert_eq!(restored.binding().route_entry, "original-member");
}

#[test]
fn authenticated_id_lookup_separates_accounts_and_refuses_adoption() {
    let r = new_run();
    let c = r
        .bind_provider(0, 2, &r.encode().unwrap(), "provider-a")
        .unwrap();
    let RecordMutation::Put { key, value, .. } = &c.candidate.mutations[1] else {
        panic!("index put")
    };
    assert!(key.starts_with(&provider_lookup_prefix(&"a".repeat(64), "provider-a").unwrap()));
    let index = ProviderIndex::decode(value, &"a".repeat(64), "provider-a").unwrap();
    assert_eq!(index.run_key(), r.key());
    assert!(ProviderIndex::decode(value, &"b".repeat(64), "provider-a").is_err());
    assert!(ProviderIndex::decode(value, &"a".repeat(64), "provider-b").is_err());
    let other = Partition::new(&"a".repeat(64), "account-b").unwrap();
    assert_ne!(
        partition().provider_key("provider-a").unwrap(),
        other.provider_key("provider-a").unwrap()
    );
    assert!(other
        .provider_key("provider-a")
        .unwrap()
        .starts_with(&provider_lookup_prefix(&"a".repeat(64), "provider-a").unwrap()));
}
#[test]
fn every_component_requires_final_nonquarantined_receipt_before_explicit_close() {
    let r = bound();
    let o = observation(&r);
    let r = r
        .final_components(0, 3, &r.encode().unwrap(), std::slice::from_ref(&o))
        .unwrap()
        .committed(&applied(3))
        .unwrap();
    assert!(matches!(r.recovery(), Recovery::Submit { .. }));
    for result in [
        ObservationResult::PendingReconciliation,
        ObservationResult::Refused,
        ObservationResult::Accepted(Receipt {
            settlement_id: [1; 16],
            duplicate: false,
            quarantined: true,
        }),
    ] {
        assert!(r.accepted(0, 4, &r.encode().unwrap(), &o, &result).is_err());
    }
    let result = ObservationResult::Accepted(Receipt {
        settlement_id: [1; 16],
        duplicate: true,
        quarantined: false,
    });
    let r = r
        .accepted(0, 4, &r.encode().unwrap(), &o, &result)
        .unwrap()
        .committed(&applied(4))
        .unwrap();
    assert_eq!(r.recovery(), Recovery::Close(ScopeRequest::close([7; 32])));
    assert!(r
        .closed(
            0,
            5,
            &r.encode().unwrap(),
            &ObservationResult::PendingReconciliation
        )
        .is_err());
    let r = r
        .closed(
            0,
            5,
            &r.encode().unwrap(),
            &ObservationResult::ScopeReady {
                scope: [7; 32],
                closed: true,
            },
        )
        .unwrap()
        .committed(&applied(5))
        .unwrap();
    assert_eq!(
        Run::decode(&r.encode().unwrap(), &partition())
            .unwrap()
            .recovery(),
        Recovery::Tombstone
    );
}
#[test]
fn deleting_content_preserves_unresolved_accounting_tombstone() {
    let r = bound();
    let r = r
        .deleted(0, 3, &r.encode().unwrap())
        .unwrap()
        .committed(&applied(3))
        .unwrap();
    assert_eq!(r.recovery(), Recovery::DeletedUnresolved);
    assert!(r.provider_id().is_some());
}
#[test]
fn stale_checkpoints_and_unknown_components_do_not_advance() {
    let r = bound();
    assert!(r.deleted(0, 3, b"stale").is_err());
    let result = ObservationResult::Accepted(Receipt {
        settlement_id: [1; 16],
        duplicate: false,
        quarantined: false,
    });
    assert!(r
        .accepted(0, 3, &r.encode().unwrap(), &observation(&r), &result)
        .is_err());
}

#[test]
fn independent_events_with_same_component_survive_restart_and_content_deletion() {
    let r = bound();
    let first = observation(&r);
    let mut second = first.clone();
    second.event_key = b"hosted-independent-interval".to_vec();
    let r = r
        .final_components(0, 3, &r.encode().unwrap(), &[first.clone(), second.clone()])
        .unwrap()
        .committed(&applied(3))
        .unwrap();
    let result = ObservationResult::Accepted(Receipt {
        settlement_id: [1; 16],
        duplicate: false,
        quarantined: false,
    });
    let r = r
        .accepted(0, 4, &r.encode().unwrap(), &first, &result)
        .unwrap()
        .committed(&applied(4))
        .unwrap();
    let r = r
        .deleted(0, 5, &r.encode().unwrap())
        .unwrap()
        .committed(&applied(5))
        .unwrap();
    let r = Run::decode(&r.encode().unwrap(), &partition()).unwrap();
    assert_eq!(
        r.recovery(),
        Recovery::Submit {
            attach: ScopeRequest::attach([7; 32]),
            observations: vec![second.clone()]
        }
    );
    let r = r
        .accepted(0, 6, &r.encode().unwrap(), &second, &result)
        .unwrap()
        .committed(&applied(6))
        .unwrap();
    assert_eq!(r.recovery(), Recovery::Close(ScopeRequest::close([7; 32])));
}
