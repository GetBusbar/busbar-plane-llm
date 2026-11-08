// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use crate::common;

use crate::exchange::reply::relay::{Relay, RelayCtx};
use busbar_contract::operation::OpVerb;

#[test]
fn chat_single_decode_preserves_validated_billing_and_faults() {
    let cases: &[(&str, &[u8])] = &[
        ("openai", br#"{"choices":[{"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#),
        ("openai", br#"{"choices":[],"usage":{"prompt_tokens":"bad","completion_tokens":2}}"#),
        ("openai", br#"{"choices":"malformed","usage":{"prompt_tokens":5,"completion_tokens":2}}"#),
        ("openai", br#"{"choices":[],"usage":{"prompt_tokens":5},"cut":"#),
        ("openai", b"not json"),
        ("responses", br#"{"status":"failed","error":{"code":"server_error"},"output":[],"usage":{"input_tokens":5,"output_tokens":2}}"#),
        ("responses", br#"{"status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":2}}"#),
    ];
    for &(protocol, body) in cases {
        for cap in [64, 4096] {
            let handler = crate::codec::DECLS
                .iter()
                .find(|d| d.name == protocol)
                .unwrap()
                .handler
                .unwrap()
                .operation_handler(OpVerb::CHAT)
                .unwrap();
            let context = || RelayCtx {
                ingress: protocol,
                egress: protocol,
                far_is_stream: false,
                json_array: false,
                client_include_usage: false,
                request: None,
                handler,
                meter: true,
                translate_cap: cap,
            };
            let mut legacy = Relay::new(context());
            let mut once = Relay::new_for_operation(context(), OpVerb::CHAT);
            for chunk in body.chunks(7) {
                let _ = legacy.feed(&common::TestHost, chunk);
                let _ = once.feed(&common::TestHost, chunk);
            }
            let expected = legacy.end(&common::TestHost);
            let actual = once.end(&common::TestHost);
            assert_eq!(
                actual.usage, expected.usage,
                "{protocol}, cap={cap}, {body:?}"
            );
            assert_eq!(actual.tap_fault, expected.tap_fault);
            assert_eq!(actual.open_units, expected.open_units);
            assert_eq!(actual.generation_failed, expected.generation_failed);
            assert_eq!(actual.failed, expected.failed);
        }
    }
}
