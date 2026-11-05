use super::*;
use crate::exchange::reply::Units;
use crate::plane_door::counts;

const TABLE: &[UsageCount] = &[
    (CountSlot::Input, CountRead::Zero(&["input"])),
    (CountSlot::Output, CountRead::Zero(&["output"])),
    (CountSlot::CacheRead, CountRead::Opt(&["cache"])),
    (CountSlot::Input, CountRead::Less(&["cache"])),
];

#[test]
fn exact_raw_decimal_reaches_the_live_abi_without_an_integer_projection() {
    for raw in [
        "27.5",
        "27.1",
        "27.0",
        "1e3",
        "0",
        "0.000001",
        "9223372036854.775807",
    ] {
        let body = format!(r#"{{"input":{raw},"output":0}}"#);
        let parsed = serde_json::from_str(&body).unwrap();
        let ir = read_usage("test", Some(&parsed), TABLE).unwrap();
        let usage = ir.to_token_usage();
        assert!(usage.refused_counts.is_empty());
        let emitted = counts(&Units::of(Some(&usage), Default::default()));
        let input = emitted.iter().find(|unit| unit.class == 0).unwrap();
        assert_eq!(input.count().unwrap(), Count::parse(raw).unwrap(), "{raw}");
        assert_eq!(
            ir.wire_count("input", ir.input_tokens).to_string(),
            Count::parse(raw).unwrap().to_decimal_string()
        );
    }
}

#[test]
fn invalid_scale_and_store_overflow_are_explicit_refusals_without_a_floor() {
    for raw in ["0.0000001", "9223372036854.775808", "9223372036855", "-1"] {
        let parsed = serde_json::from_str(&format!(r#"{{"input":{raw},"output":27.1}}"#)).unwrap();
        let usage = read_usage("test", Some(&parsed), TABLE)
            .unwrap()
            .to_token_usage();
        assert!(!usage.refused_counts.is_empty(), "{raw}");
        assert!(!usage.exact_counts.as_ref().unwrap().contains_key("input"));
        let units = Units::of(Some(&usage), Default::default());
        assert!(!units.refused_counts.is_empty());
        assert!(!units.floor);
        let emitted = counts(&units);
        assert!(emitted.iter().all(|unit| unit.class != 0));
        assert_eq!(
            emitted
                .iter()
                .find(|unit| unit.class == 1)
                .unwrap()
                .count()
                .unwrap(),
            Count::parse("27.1").unwrap()
        );
    }
}

#[test]
fn fractional_cache_normalization_and_total_reconciliation_stay_exact() {
    let parsed =
        serde_json::from_str(r#"{"input":27.5,"output":27.1,"cache":0.5,"total":55.1}"#).unwrap();
    let mut ir = read_usage("test", Some(&parsed), TABLE).unwrap();
    assert!(stated_total_note("test", "test.usage", parsed.get("total"), &mut ir).is_none());
    let exact = ir.to_token_usage().exact_counts.unwrap();
    assert_eq!(exact["input"], Count::parse("27").unwrap());
    assert_eq!(exact["cache_read"], Count::parse("0.5").unwrap());
    assert_eq!(exact["unitemized_tokens"], Count::parse("0.5").unwrap());
    assert_eq!(
        ir.wire_total(&["input", "output", "cache_read"], 0)
            .to_string(),
        "54.6"
    );
}

#[test]
fn every_chat_writer_emits_exact_decimal_totals() {
    let raw = serde_json::from_str(r#"{"input":27.5,"output":27.1}"#).unwrap();
    let ir = crate::codec::ir::IrResponse {
        id: Some("count-test".to_owned()),
        model: Some("count-model".to_owned()),
        usage: read_usage("test", Some(&raw), TABLE).unwrap(),
        ..Default::default()
    };
    for (protocol, input, output) in [
        ("anthropic", "/usage/input_tokens", "/usage/output_tokens"),
        ("openai", "/usage/prompt_tokens", "/usage/completion_tokens"),
        ("responses", "/usage/input_tokens", "/usage/output_tokens"),
        (
            "gemini",
            "/usageMetadata/promptTokenCount",
            "/usageMetadata/candidatesTokenCount",
        ),
        (
            "cohere",
            "/usage/tokens/input_tokens",
            "/usage/tokens/output_tokens",
        ),
        ("bedrock", "/usage/inputTokens", "/usage/outputTokens"),
    ] {
        let codec = crate::codec::proto_codec::protocol_for(protocol).unwrap();
        let wire = codec
            .writer()
            .write_response(crate::codec::test_host::HOST, &ir);
        let bytes = serde_json::to_vec(&wire).unwrap();
        assert_eq!(
            Count::read_at(&bytes, input).unwrap(),
            Some(Count::parse("27.5").unwrap()),
            "{protocol}: {wire}"
        );
        assert_eq!(
            Count::read_at(&bytes, output).unwrap(),
            Some(Count::parse("27.1").unwrap()),
            "{protocol}: {wire}"
        );
    }
}

#[test]
fn leaf_counts_preserve_fractions_and_refusals_through_live_units() {
    for class in ["input", "search_units"] {
        for raw in ["27.5", "27.1", "27.0", "1e3", "0"] {
            let body = serde_json::from_str(&format!(r#"{{"count":{raw}}}"#)).unwrap();
            let usage = token_usage("test", Some(&body), &[("count", class)]).unwrap();
            let units = counts(&Units::of(Some(&usage), Default::default()));
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].count().unwrap(), Count::parse(raw).unwrap());
        }
        let body = serde_json::from_str(r#"{"count":0.0000001}"#).unwrap();
        let usage = token_usage("test", Some(&body), &[("count", class)]).unwrap();
        assert!(!usage.refused_counts.is_empty());
        assert!(counts(&Units::of(Some(&usage), Default::default())).is_empty());
    }
}
