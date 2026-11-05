use super::*;

#[path = "../../../testing/settings/integer_token.rs"]
mod settings_profile;

#[test]
fn settings_schema_matches_lifecycle_grammar() {
    assert_eq!(STATEMENT.settings_schema.ptr, SETTINGS_SCHEMA.as_ptr());
    assert_eq!(STATEMENT.settings_schema.len, SETTINGS_SCHEMA.len());
    let schema: Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
    let validator = settings_profile::validator(&schema).unwrap();
    for (source, accepted) in [
        (r#"{}"#, true),
        (r#"null"#, true),
        (r#"{"providers":null,"models":null,"limits":null}"#, true),
        (
            r#"{"providers":{"p":{"protocol":"openai","OpenAI-Project":"project_1"}},"models":{"m":{"provider":"p"}},"limits":{"default_max_tokens":50},"pools":{"p":{"members":["m",{"model":"m","reasoning":true}]}}}"#,
            true,
        ),
        (
            r#"{"providers":{"p":{"anthropic-beta":["feature-1","feature-2"]}}}"#,
            true,
        ),
        (
            r#"{"providers":{"p":{"protocol":"gemini","x-server-timeout":0}}}"#,
            true,
        ),
        (r#"{"providers":{"p":{"protocol":"not-yet-used"}}}"#, true),
        (r#"{"pools":{"p":{"members":false}},"ignored":true}"#, true),
        (r#"{"providers":[]}"#, false),
        (r#"{"models":{"m":{}}}"#, false),
        (
            r#"{"providers":{"p":{"protocol":"openai","OpenAI-Project":"bad project"}}}"#,
            false,
        ),
        (
            r#"{"providers":{"p":{"protocol":"openai","anthropic-beta":"feature"}}}"#,
            false,
        ),
        (r#"{"providers":{"p":{"Authorization":"secret"}}}"#, false),
        (
            r#"{"providers":{"p":{"model_capabilities":[{"models":"not-a-list"}]}}}"#,
            false,
        ),
        (r#"{"providers":{"p":{"max_output_key":"typo"}}}"#, false),
        (r#"{"limits":{"default_max_tokens":-1}}"#, false),
        (r#"{"pools":{"p":{"members":[{}]}}}"#, false),
    ] {
        let value: Value = serde_json::from_str(source).unwrap();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            read_settings(&bytes).is_ok(),
            accepted,
            "lifecycle: {source}"
        );
        assert_eq!(validator.is_valid(&value), accepted, "schema: {source}");
    }
}

/// Every declared provider option must be represented in the lifecycle schema.
#[test]
fn settings_schema_covers_provider_option_catalogue() {
    let schema: Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
    let validator = settings_profile::validator(&schema).unwrap();
    for dialect in [
        "anthropic",
        "openai",
        "responses",
        "gemini",
        "bedrock",
        "cohere",
    ] {
        for option in crate::options::catalogue(dialect) {
            use crate::options::Check;
            let value = match option.check {
                Check::Token => serde_json::json!("token-1"),
                Check::Text => serde_json::json!("some text"),
                Check::OneOf(words) => serde_json::json!(words[0]),
                Check::Seconds => serde_json::json!(1),
                Check::TokenList => serde_json::json!(["feature-1"]),
            };
            let mut provider = serde_json::Map::new();
            provider.insert("protocol".into(), serde_json::json!(dialect));
            provider.insert(option.name.into(), value);
            let settings = serde_json::json!({"providers": {"p": provider}});
            assert!(read_settings(&serde_json::to_vec(&settings).unwrap()).is_ok());
            assert!(
                validator.is_valid(&settings),
                "{} / {}",
                dialect,
                option.name
            );
        }
    }
}

#[test]
fn settings_schema_describes_owned_webhook_switches_separately() {
    let schema: Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema["$defs"]["owned_sections"]).unwrap();
    for (source, accepted) in [
        (r#"{}"#, true),
        (r#"{"webhooks":null}"#, true),
        (r#"{"webhooks":{}}"#, true),
        (r#"{"webhooks":{"openai_responses":true}}"#, true),
        (r#"{"webhooks":{"openai_responses":"true"}}"#, false),
        (r#"{"webhooks":{"typo":true}}"#, false),
    ] {
        let value: Value = serde_json::from_str(source).unwrap();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            webhook::from_owned(&bytes).is_ok(),
            accepted,
            "lifecycle: {source}"
        );
        assert_eq!(validator.is_valid(&value), accepted, "schema: {source}");
    }
}

#[test]
fn settings_schema_integer_tokens_match_native_parser() {
    let schema: serde_json::Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
    let profile = settings_profile::validator(&schema).unwrap();
    let standard = jsonschema::validator_for(&schema).unwrap();
    assert_eq!(schema["x-busbar-settings-profile"], "integer-token-v1");
    for (pointer, template, integer, kind) in [
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/default_max_tokens/anyOf/0"#,
            r#"{"limits":{"default_max_tokens":TOKEN}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/reasoning_effort_budgets/anyOf/0/properties/minimal/anyOf/0"#,
            r#"{"limits":{"reasoning_effort_budgets":{"minimal":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/reasoning_effort_budgets/anyOf/0/properties/low/anyOf/0"#,
            r#"{"limits":{"reasoning_effort_budgets":{"low":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/reasoning_effort_budgets/anyOf/0/properties/medium/anyOf/0"#,
            r#"{"limits":{"reasoning_effort_budgets":{"medium":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/reasoning_effort_budgets/anyOf/0/properties/high/anyOf/0"#,
            r#"{"limits":{"reasoning_effort_budgets":{"high":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/anyOf/0/properties/limits/anyOf/0/properties/request_body_max_bytes/anyOf/0"#,
            r#"{"limits":{"request_body_max_bytes":TOKEN}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/$defs/provider/allOf/1/anyOf/2/patternProperties/^[xX]\-[sS][eE][rR][vV][eE][rR]\-[tT][iI][mM][eE][oO][uU][tT]$/anyOf/0"#,
            r#"{"providers":{"p":{"protocol":"gemini","x-server-timeout":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
        (
            r#"/$defs/model/properties/default_max_tokens/anyOf/0"#,
            r#"{"providers":{"p":{"protocol":"openai"}},"models":{"m":{"provider":"p","default_max_tokens":TOKEN}}}"#,
            "1",
            "unsigned",
        ),
    ] {
        assert_eq!(
            schema.pointer(pointer).unwrap()["x-busbar-integer-token"],
            kind
        );
        for (token, accepted) in [
            (integer.to_string(), true),
            (format!("{integer}.0"), false),
            (format!("{integer}e0"), false),
            (format!("{integer}E+0"), false),
        ] {
            let text = template.replace("TOKEN", &token);
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                standard.is_valid(&value),
                "standard mathematical integer: {text}"
            );
            assert_eq!(profile.is_valid(&value), accepted, "profile: {text}");
            assert_eq!(
                read_settings(text.as_bytes()).is_ok(),
                accepted,
                "native: {text}"
            );
        }
        // Do not infer signed negative-zero behavior from a regular expression:
        // the profile must agree with the actual lifecycle parser.
        for token in [
            "null",
            "0",
            "-0",
            "-0.0",
            "0e0",
            "-1",
            "18446744073709551616",
        ] {
            let text = template.replace("TOKEN", token);
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                profile.is_valid(&value),
                read_settings(text.as_bytes()).is_ok(),
                "native sign/range: {text}"
            );
        }
    }
}

#[test]
fn settings_schema_gemini_timeout_preserves_u64_and_signed_zero() {
    let schema: Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
    let profile = settings_profile::validator(&schema).unwrap();
    // section::<BTreeMap<String, Map<String, Value>>> reconstructs provider values
    // before options::read, admitting integer -0 as unsigned zero. Retain the
    // unsigned upper half as well as this exact signed-zero representation.
    for (token, accepted) in [
        ("-0", true),
        ("0", true),
        ("9223372036854775808", true),
        ("18446744073709551615", true),
        ("-1", false),
        ("-0.0", false),
        ("0e0", false),
        ("1.0", false),
        ("1e0", false),
        ("18446744073709551616", false),
    ] {
        let text = r#"{"providers":{"p":{"protocol":"gemini","x-server-timeout":TOKEN}}}"#
            .replace("TOKEN", token);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(profile.is_valid(&value), accepted, "profile: {text}");
        assert_eq!(
            read_settings(text.as_bytes()).is_ok(),
            accepted,
            "native: {text}"
        );
    }
}
