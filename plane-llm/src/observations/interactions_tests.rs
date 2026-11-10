use super::*;
use serde_json::json;

pub(super) fn binding() -> Binding {
    Binding {
        scope: [7; 32],
        account_namespace: "account-a".into(),
        route_entry: "original-member".into(),
        model: "gemini-pinned".into(),
        pricing_mode: "standard".into(),
        usage_schema: "gemini-interactions-2026-11-10".into(),
        input_class: 0,
        cache_read_class: 2,
        output_class: 1,
        tool_use_class: Some(22),
        priced_classes: vec![0, 1, 2, 22],
    }
}
pub(super) fn body() -> Value {
    json!({"id":"interaction-a","model":"gemini-pinned","status":"completed","usage":{"total_input_tokens":100,"total_cached_tokens":20,"total_output_tokens":30,"total_thought_tokens":40,"total_tool_use_tokens":5,"total_tokens":170,"input_tokens_by_modality":[{"modality":"text","tokens":100}]}})
}
fn normalized(binding: &Binding, body: &Value) -> Normalized {
    normalize(
        binding,
        Evidence {
            known_id: Some("interaction-a"),
            http_status: 200,
            body: &serde_json::to_vec(body).unwrap(),
            evidence_ref: b"protected/ref",
        },
    )
}
fn finals(result: Normalized) -> Vec<Observation> {
    let Normalized::Final { observations, .. } = result else {
        panic!("{result:?}")
    };
    observations
}

#[test]
fn final_components_are_disjoint_and_thoughts_are_additional_output() {
    let rows = finals(normalized(&binding(), &body()));
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].counts[&0], Count::from(80));
    assert_eq!(rows[1].counts[&2], Count::from(20));
    assert_eq!(rows[2].counts[&1], Count::from(30));
    assert_eq!(rows[3].counts[&1], Count::from(40));
    assert_eq!(rows[4].counts[&22], Count::from(5));
    assert!(rows.iter().all(|o| o.revision.is_none()));
}

#[test]
fn native_tier_usage_requires_confirmation_and_uses_only_its_original_classes() {
    for tier in ["flex", "priority"] {
        let classes = super::super::profiles::text_classes(tier).unwrap();
        let mut binding = binding();
        binding.pricing_mode = tier.into();
        binding.input_class = classes.input;
        binding.output_class = classes.output;
        binding.cache_read_class = classes.cache_read;
        binding.tool_use_class = None;
        binding.priced_classes = classes.required().to_vec();
        let mut body = body();
        body["usage"]["total_tool_use_tokens"] = json!(0);
        assert!(matches!(
            normalized(&binding, &body),
            Normalized::Pending(_)
        ));
        body["service_tier"] = json!("standard");
        assert!(matches!(
            normalized(&binding, &body),
            Normalized::Pending(_)
        ));
        body["service_tier"] = json!(tier);
        let rows = finals(normalized(&binding, &body));
        assert_eq!(rows[0].counts[&classes.input], Count::from(80));
        assert_eq!(rows[1].counts[&classes.cache_read], Count::from(20));
        assert_eq!(rows[2].counts[&classes.output], Count::from(30));
        assert_eq!(rows[3].counts[&classes.output], Count::from(40));
        assert!(rows.iter().all(|r| r.counts.keys().all(|c| *c >= 22)));
        binding.priced_classes.retain(|c| *c != classes.cache_read);
        assert!(matches!(
            normalized(&binding, &body),
            Normalized::Pending(_)
        ));
    }
}
#[test]
fn repeated_polls_and_sse_replay_produce_identical_economic_observations() {
    let expected = finals(normalized(&binding(), &body()));
    for _ in 0..100 {
        assert_eq!(finals(normalized(&binding(), &body())), expected);
    }
    let event = json!({"event_type":"interaction.completed","event_id":"opaque-not-a-revision","interaction":body()});
    assert_eq!(finals(normalized(&binding(), &event)), expected);
}
#[test]
fn continuation_id_and_account_namespace_are_distinct() {
    let a = finals(normalized(&binding(), &body()));
    let mut different = binding();
    different.account_namespace = "account-b".into();
    let b = finals(normalized(&different, &body()));
    assert_ne!(a[0].event_key, b[0].event_key);
    let mut value = body();
    value["id"] = json!("interaction-b");
    let c = finals(normalize(
        &binding(),
        Evidence {
            known_id: Some("interaction-b"),
            http_status: 200,
            body: &serde_json::to_vec(&value).unwrap(),
            evidence_ref: b"protected/ref",
        },
    ));
    assert_ne!(a[0].event_key, c[0].event_key);
}
#[test]
fn failed_cancelled_and_incomplete_with_usage_are_billable() {
    for status in ["failed", "cancelled", "incomplete", "budget_exceeded"] {
        let mut value = body();
        value["status"] = json!(status);
        assert_eq!(finals(normalized(&binding(), &value)).len(), 5);
    }
}
#[test]
fn terminal_without_usage_can_later_finalize() {
    let mut value = body();
    value.as_object_mut().unwrap().remove("usage");
    assert!(matches!(
        normalized(&binding(), &value),
        Normalized::Pending(_)
    ));
    assert_eq!(finals(normalized(&binding(), &body())).len(), 5);
}
#[test]
fn unknown_owner_and_failed_get_do_not_create_observations() {
    assert_eq!(
        normalize(
            &binding(),
            Evidence {
                known_id: None,
                http_status: 200,
                body: b"{}",
                evidence_ref: b"ref"
            }
        ),
        Normalized::Unattributed
    );
    for http_status in [403, 404, 500] {
        assert_eq!(
            normalize(
                &binding(),
                Evidence {
                    known_id: Some("interaction-a"),
                    http_status,
                    body: b"{}",
                    evidence_ref: b"ref"
                }
            ),
            Normalized::RetrievalFailed
        );
    }
}
#[test]
fn malformed_missing_fractional_negative_and_inconsistent_counts_are_unresolved() {
    for value in [
        json!(-1),
        json!(null),
        json!("1"),
        json!(1.25),
        json!("18446744073709551616"),
        json!({"$serde_json::private::Number":"0"}),
    ] {
        let mut b = body();
        b["usage"]["total_cached_tokens"] = value;
        assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    }
    let mut b = body();
    b["usage"]["total_cached_tokens"] = json!(101);
    assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    b = body();
    b["usage"]
        .as_object_mut()
        .unwrap()
        .remove("total_thought_tokens");
    assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    b = body();
    b["usage"]["input_tokens_by_modality"][0]["tokens"] = json!(99);
    assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
}
#[test]
fn explicit_zero_is_distinct_from_missing() {
    let mut b = body();
    b["usage"]["total_thought_tokens"] = json!(0);
    let rows = finals(normalized(&binding(), &b));
    assert_eq!(rows[3].counts[&1], Count::ZERO);
}
#[test]
fn unknown_model_modality_and_missing_tariff_remain_unresolved() {
    let mut changed_tier = body();
    changed_tier["service_tier"] = json!("flex");
    assert!(matches!(
        normalized(&binding(), &changed_tier),
        Normalized::Pending(_)
    ));
    let mut b = body();
    b["model"] = json!("cheaper-model");
    assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    b = body();
    b["usage"]["input_tokens_by_modality"][0]["modality"] = json!("audio");
    assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    let mut config = binding();
    config.tool_use_class = None;
    assert!(matches!(
        normalized(&config, &body()),
        Normalized::Pending(_)
    ));
    config = binding();
    config.priced_classes.retain(|c| *c != 1);
    assert!(matches!(
        normalized(&config, &body()),
        Normalized::Pending(_)
    ));
}
#[test]
fn nonfinal_and_status_only_events_cannot_settle() {
    for status in [
        "in_progress",
        "requires_action",
        "requires_continuation",
        "unknown",
    ] {
        let mut b = body();
        b["status"] = json!(status);
        assert!(matches!(normalized(&binding(), &b), Normalized::Pending(_)));
    }
    assert!(matches!(
        normalized(
            &binding(),
            &json!({"event_type":"interaction.completed","interaction":{"id":"interaction-a","status":"completed"}})
        ),
        Normalized::Pending(_)
    ));
}
#[test]
fn native_operation_table_preserves_retrieval_identity() {
    assert_eq!(
        operation("POST", "/v1beta/interactions"),
        Ok(Operation::Create)
    );
    assert_eq!(
        operation(
            "GET",
            "/v1beta/interactions/abc?stream=true&last_event_id=evt-1"
        ),
        Ok(Operation::Get {
            id: "abc".into(),
            stream: true,
            last_event_id: Some("evt-1".into())
        })
    );
    assert_eq!(
        operation("POST", "/v1beta/interactions/abc/cancel"),
        Ok(Operation::Cancel { id: "abc".into() })
    );
    assert_eq!(
        operation("DELETE", "/v1beta/interactions/abc"),
        Ok(Operation::Delete { id: "abc".into() })
    );
    for target in [
        "/v1beta/interactions/abc?last_event_id=evt",
        "/v1beta/interactions/abc?stream=true&stream=false",
        "/v1beta/interactions/abc%2fdef",
        "/v1beta/interactions/../escape",
    ] {
        assert!(operation("GET", target).is_err());
    }
}

#[test]
fn create_validates_reconcilable_model_variant_without_reusing_previous_scope() {
    let request = create(br#"{"model":"entry","input":"next","previous_interaction_id":"older-run","background":true,"stream":true}"#).unwrap();
    assert_eq!(
        request.previous_interaction_id.as_deref(),
        Some("older-run")
    );
    assert!(request.background && request.stream);
    for body in [
        br#"{"model":"entry","store":false}"#.as_slice(),
        br#"{"agent":"unknown"}"#,
        br#"{"model":"entry","continuation_token":"opaque"}"#,
        br#"{"model":"entry","background":"true"}"#,
    ] {
        assert!(create(body).is_err());
    }
}

#[test]
fn cached_modality_partition_is_validated_without_adding_to_token_total() {
    let mut snapshot = body();
    snapshot["usage"]["cached_tokens_by_modality"] = json!([{"modality":"text","tokens":20}]);
    assert_eq!(
        finals(normalized(&binding(), &snapshot))[1].counts[&2],
        Count::from(20)
    );
    snapshot["usage"]["cached_tokens_by_modality"] = json!([{"modality":"text","tokens":21}]);
    assert!(matches!(
        normalized(&binding(), &snapshot),
        Normalized::Pending(_)
    ));
    snapshot["usage"]["cached_tokens_by_modality"] = json!([{"modality":"image","tokens":20}]);
    assert!(matches!(
        normalized(&binding(), &snapshot),
        Normalized::Pending(_)
    ));
}
