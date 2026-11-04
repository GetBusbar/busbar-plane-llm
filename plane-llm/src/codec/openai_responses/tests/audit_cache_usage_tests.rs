use super::*;
#[test]
fn responses_usage_preserves_read_and_write_cache_categories() {
    let wire = serde_json::json!({"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":20,"cache_write_tokens":30}});
    let usage = read_responses_usage(Some(&wire), None).unwrap();
    assert_eq!(usage.input_tokens, 50);
    assert_eq!(usage.cache_read_input_tokens, Some(20));
    assert_eq!(usage.cache_creation_input_tokens, Some(30));
    assert_eq!(usage.output_tokens, 7);
}
