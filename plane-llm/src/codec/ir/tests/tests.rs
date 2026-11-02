use super::*;
use serde_json::Value;

#[test]
fn reasoning_effort_parse_round_trips_and_rejects_unknown() {
    for w in ["minimal", "low", "medium", "high"] {
        assert_eq!(IrReasoningEffort::parse(w).unwrap().as_str(), w);
    }
    assert!(IrReasoningEffort::parse("verylow").is_none());
    assert!(IrReasoningEffort::parse("").is_none());

    assert_eq!(IrReasoningEffort::Minimal.as_three_word_str(), "low");
    assert_eq!(IrReasoningEffort::High.as_three_word_str(), "high");
}

#[test]
fn reasoning_ask_to_budget_and_to_effort_use_the_table() {
    let table = [1000u32, 4000, 8000, 16000];
    use IrReasoningAsk::*;
    use IrReasoningEffort::*;

    assert_eq!(Effort(Minimal).to_budget(table), Some(1000));
    assert_eq!(Effort(High).to_budget(table), Some(16000));

    assert_eq!(Dynamic.to_budget(table), None);

    assert_eq!(Budget(1234).to_budget(table), Some(1234));

    assert_eq!(Budget(500).to_effort(table), Some(Minimal));
    assert_eq!(Budget(4000).to_effort(table), Some(Low));
    assert_eq!(Budget(8001).to_effort(table), Some(Medium));
    assert_eq!(Budget(99999).to_effort(table), Some(High));

    assert_eq!(Dynamic.to_effort(table), None);
    assert_eq!(Effort(High).to_effort(table), Some(High));
}

/// `billable_tokens` sums all four token fields with `saturating_add` (the operands are

#[test]
fn test_billable_tokens_sum_and_saturation() {
    let u = IrUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: Some(3),
        cache_creation_input_tokens: Some(2),
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    assert_eq!(u.billable_tokens(), 20);

    let z = IrUsage::default();
    assert_eq!(z.billable_tokens(), 0);

    let big = IrUsage {
        input_tokens: u64::MAX,
        output_tokens: 1,
        cache_read_input_tokens: Some(1),
        cache_creation_input_tokens: Some(1),
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    assert_eq!(big.billable_tokens(), u64::MAX);
}

#[test]
fn test_stream_decode_state_default() {
    let st = StreamDecodeState::default();
    assert!(!st.started);
    assert!(!st.text_block_open);
    assert!(st.text_index.is_none());
    assert!(st.open_tools.is_empty());
    assert!(!st.reasoning_seen);
    assert!(!st.thinking_block_open);
    assert!(st.pending_stop_reason.is_none());
    assert!(st.tool_ir_index.is_empty());
}

#[test]
fn test_read_stop_sequences_drops_empty_strings() {
    let bare_empty = Value::String(String::new());
    assert!(
        read_stop_sequences(Some(&bare_empty)).is_empty(),
        "bare empty string should collapse to empty Vec (== omitted)"
    );

    let arr_empty = Value::Array(vec![Value::String(String::new())]);
    assert!(
        read_stop_sequences(Some(&arr_empty)).is_empty(),
        "[\"\"] should collapse to empty Vec (== omitted)"
    );

    let mixed = Value::Array(vec![
        Value::String("STOP".into()),
        Value::String(String::new()),
        Value::Null,
        Value::String("END".into()),
    ]);
    assert_eq!(
        read_stop_sequences(Some(&mixed)),
        vec!["STOP".to_string(), "END".to_string()],
        "empty/non-string elements dropped; real stops kept in order"
    );

    let bare = Value::String("HALT".into());
    assert_eq!(read_stop_sequences(Some(&bare)), vec!["HALT".to_string()]);
    assert!(read_stop_sequences(None).is_empty());
}

/// THE THREE OPAQUE SHAPES, read through the REAL readers, asserted from the IR ALONE.

#[test]
fn all_three_opaque_reasoning_shapes_read_as_opaque_from_the_ir() {
    let cases: [(&str, crate::codec::proto_codec::Protocol, Value); 3] = [
        (
            "anthropic redacted_thinking",
            crate::codec::proto_codec::Protocol::anthropic(),
            serde_json::json!({
                "model": "claude", "max_tokens": 16,
                "messages": [{"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "OPAQUE_BLOB"}
                ]}]
            }),
        ),
        (
            "bedrock redactedContent",
            crate::codec::proto_codec::Protocol::bedrock(),
            serde_json::json!({
                "messages": [{"role": "assistant", "content": [
                    {"reasoningContent": {"redactedContent": "OPAQUE_BLOB"}}
                ]}]
            }),
        ),
        (
            "responses encrypted_content-only",
            crate::codec::proto_codec::Protocol::responses(),
            serde_json::json!({
                "model": "gpt-5",
                "input": [{
                    "type": "reasoning", "content": [], "encrypted_content": "OPAQUE_BLOB"
                }]
            }),
        ),
    ];

    for (name, proto, body) in cases {
        let ir = proto
            .reader()
            .read_request(&body)
            .unwrap_or_else(|e| panic!("{name}: the reader rejected the fixture: {e:?}"));
        let block = ir
            .messages
            .first()
            .and_then(|m| m.content.first())
            .unwrap_or_else(|| panic!("{name}: expected one content block in the IR"));
        assert!(
            block.is_opaque(),
            "{name}: the IR must report this block as OPAQUE. A consumer that reads opacity off \
             the IR and gets `false` here shows an EMPTY turn for a request the provider receives \
             in full — the policy-enforcement-point bypass this predicate exists to close. Got: \
             {block:?}"
        );
    }
}

/// The negative half, so `is_opaque` cannot pass the test above by answering `true` to everything.

#[test]
fn readable_blocks_are_not_opaque() {
    let cases = [
        IrBlock::text("hello"),
        IrBlock::Thinking {
            text: "step one".into(),
            signature: None,
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin: None,
        },
        IrBlock::Thinking {
            text: "step one".into(),
            signature: Some("SIG".into()),
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin: None,
        },
        IrBlock::Thinking {
            text: String::new(),
            signature: None,
            redacted: false,
            cache_control: None,
            kind: None,
            signature_origin: None,
        },
    ];
    for block in cases {
        assert!(
            !block.is_opaque(),
            "a block busbar CAN read must not be reported opaque: {block:?}"
        );
    }
}

#[test]
fn cohere_bills_its_raw_tokens_whatever_billed_units_says() {
    let raw_wins_when_absent = IrUsage {
        input_tokens: 100,
        output_tokens: 40,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    let tu = raw_wins_when_absent.to_token_usage();
    assert_eq!(
        (tu.input, tu.output),
        (100, 40),
        "no billed_* → raw totals unchanged"
    );

    let billed = IrUsage {
        input_tokens: 100,
        output_tokens: 40,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail {
            billed_input_tokens: Some(90),
            billed_output_tokens: Some(35),
            ..Default::default()
        },
    };
    let tu = billed.to_token_usage();
    assert_eq!(
        (tu.input, tu.output),
        (100, 40),
        "billed_units present → the ledger still bills the RAW tokens bucket, as 1.5.5 did"
    );

    let tier = crate::codec::usage_census::tier_usage(&tu);
    let u = |k: &str| tier.usage_units.get(k).copied().unwrap_or(0);
    assert_eq!(
        (
            u(busbar_contract::records::UNIT_INPUT),
            u(busbar_contract::records::UNIT_OUTPUT)
        ),
        (100, 40)
    );
}

#[test]
fn q57_slot_vocabulary_round_trips_and_refuses_unknown_words() {
    for t in [
        IrServiceTier::Auto,
        IrServiceTier::Default,
        IrServiceTier::Flex,
        IrServiceTier::Scale,
        IrServiceTier::Priority,
    ] {
        assert_eq!(IrServiceTier::parse(t.as_str()), Some(t));
    }
    assert_eq!(IrServiceTier::parse("standard_only"), None);
    for v in [IrVerbosity::Low, IrVerbosity::Medium, IrVerbosity::High] {
        assert_eq!(IrVerbosity::parse(v.as_str()), Some(v));
    }
    assert_eq!(IrVerbosity::parse("max"), None);
    for d in [IrImageDetail::Auto, IrImageDetail::Low, IrImageDetail::High] {
        assert_eq!(IrImageDetail::parse(d.as_str()), Some(d));
    }
    assert_eq!(IrImageDetail::parse("original"), None);

    assert_eq!(IrModality::parse("AUDIO"), Some(IrModality::Audio));
    assert_eq!(IrModality::parse("text"), Some(IrModality::Text));
    assert_eq!(IrModality::parse("video"), None);
    assert_eq!(IrSystemRole::Developer.as_str(), "developer");
    assert_eq!(
        IrHostedTool::WebSearch(IrWebSearch::default()).kind_str(),
        "web_search"
    );
}

#[test]
fn q57_defaults_are_absent_values() {
    let r = IrResponse::default();
    assert_eq!(r.role, IrRole::Assistant);
    assert!(r.content.is_empty() && r.stop_reason.is_none() && r.logprobs.is_empty());
    assert!(r.id.is_none() && r.model.is_none() && r.request_echo.is_none());
    assert_eq!(r.usage, IrUsage::default());
    assert_eq!(r.usage.billable_tokens(), 0);
    let t = IrTool::default();
    assert!(t.name.is_empty() && t.hosted.is_none() && t.strict.is_none());
    assert_eq!(t.input_schema, Value::Null);
    assert_eq!(IrCitation::default().raw, None);
}

#[test]
fn ir09_reasoning_off_and_effort_above_high() {
    use IrReasoningAsk::*;
    use IrReasoningEffort::*;
    let table = [1000u32, 4000, 8000, 16000];
    assert_eq!(IrReasoningEffort::parse("xhigh"), None);
    assert_eq!(IrReasoningEffort::parse_extended("xhigh"), Some(XHigh));
    assert_eq!(IrReasoningEffort::parse_extended("max"), Some(Max));
    assert_eq!(IrReasoningEffort::parse_extended("high"), Some(High));
    assert_eq!(XHigh.as_str(), "xhigh");
    assert_eq!(Max.as_str(), "max");
    assert_eq!(XHigh.as_three_word_str(), "high");
    assert_eq!(Max.as_three_word_str(), "high");
    assert_eq!(Effort(XHigh).to_budget(table), Some(16000));
    assert_eq!(Effort(Max).to_budget(table), Some(16000));
    assert_eq!(Off.to_budget(table), None);
    assert_eq!(Off.to_effort(table), None);

    assert_eq!(Budget(u32::MAX).to_effort(table), Some(High));
}

#[test]
fn usage_by_modality_never_moves_a_billed_figure() {
    let plain = IrUsage {
        input_tokens: 120,
        output_tokens: 40,
        cache_read_input_tokens: Some(20),
        cache_creation_input_tokens: Some(5),
        ..Default::default()
    };
    let mut split = plain.clone();
    split.detail.by_modality = Some(IrUsageByModality {
        input: IrModalityCounts {
            text: Some(100),
            image: Some(20),
            ..Default::default()
        },
        output: IrModalityCounts {
            text: Some(30),
            audio: Some(10),
            ..Default::default()
        },
        cache: IrModalityCounts {
            text: Some(20),
            ..Default::default()
        },
    });
    assert_eq!(
        format!("{:?}", plain.to_token_usage()),
        format!("{:?}", split.to_token_usage())
    );
    assert_eq!(plain.billable_tokens(), split.billable_tokens());
}
