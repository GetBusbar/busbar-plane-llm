// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use crate::common;

use crate::exchange::reply::relay::{Relay, RelayCtx};
use busbar_contract::operation::OpVerb;

#[test]
fn capped_titan_embedding_preserves_root_input_tokens_for_every_chunk_size() {
    let handler = crate::codec::DECLS
        .iter()
        .find(|d| d.name == "bedrock")
        .unwrap()
        .handler
        .unwrap()
        .operation_handler(OpVerb::EMBEDDINGS)
        .unwrap();
    let body = format!(
        r#"{{"embedding":[{}],"inputTextTokenCount":5}}"#,
        vec!["0.25"; 200].join(",")
    );
    assert!(body.len() > 128);
    for chunk in [1, 2, 7, 128, body.len()] {
        let mut relay = Relay::new_for_operation(
            RelayCtx {
                ingress: "bedrock",
                egress: "bedrock",
                far_is_stream: false,
                json_array: false,
                client_include_usage: false,
                request: None,
                handler,
                meter: true,
                translate_cap: 128,
            },
            OpVerb::EMBEDDINGS,
        );
        for piece in body.as_bytes().chunks(chunk) {
            let _ = relay.feed(&common::TestHost, piece);
        }
        let end = relay.end(&common::TestHost);
        let usage = end.usage.expect("Titan reports its root input token count");
        assert_eq!((usage.input, usage.output), (5, 0));
        assert!(end.tap_fault.is_none());
    }
}
