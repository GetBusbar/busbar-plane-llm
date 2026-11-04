use super::*;
use crate::codec::cells::OperationHandler;
#[test]
fn refused_embedding_retains_provider_billing() {
    let body = br#"{"embedding":[0.1,"bad",0.3],"inputTextTokenCount":5}"#;
    assert!(EMB
        .read_response(crate::codec::test_host::HOST, body)
        .is_err());
    let usage = EMB
        .extract_usage(crate::codec::test_host::HOST, "bedrock", body)
        .unwrap()
        .unwrap();
    assert_eq!(usage.input, 5);
    assert_eq!(
        EMB.recover_response_billing(crate::codec::test_host::HOST, body),
        Some(busbar_contract::billing::Billing::Tokens(usage))
    );
}
