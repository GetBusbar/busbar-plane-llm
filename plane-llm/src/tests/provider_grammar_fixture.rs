#[test]
fn the_exported_descriptor_matches_the_provenance_bound_config_fixture() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../testing/provider-keys-llm.json")).unwrap();
    let text = fixture["descriptor_text"].as_str().unwrap();
    assert_eq!(
        busbar_contract::sha256_hex(text.as_bytes()),
        fixture["provenance"]["sha256"].as_str().unwrap()
    );
    assert_eq!(super::PROVIDER_KEYS, text);
    let statement = crate::plane_door::STATEMENT;

    let exported = crate::plane_door::EXTENSIONS;
    assert_eq!(statement.extensions.ptr, exported.as_ptr());
    assert_eq!(statement.extensions.len, exported.len());
    let actual = busbar_contract::provider_keys::stated(exported)
        .unwrap()
        .unwrap();
    let expected = busbar_contract::provider_keys::decode(text.as_bytes()).unwrap();
    assert_eq!(actual, expected);
}
