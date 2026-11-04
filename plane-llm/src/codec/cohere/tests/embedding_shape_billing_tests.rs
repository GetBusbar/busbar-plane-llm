use super::*;
use busbar_contract::codec::OperationHandler;

#[test]
fn malformed_embedding_shape_retains_reported_billing() {
    let body = br#"{"embeddings":{"float":null},"meta":{"billed_units":{"input_tokens":9}}}"#;
    assert!(read_embeddings_response(body).is_err());
    let usage = EMB
        .extract_usage(crate::codec::test_host::HOST, "cohere", body)
        .unwrap()
        .unwrap();
    assert_eq!(usage.input, 9);
    assert_eq!(
        EMB.recover_response_billing(crate::codec::test_host::HOST, body),
        Some(busbar_contract::billing::Billing::Tokens(usage))
    );
}
