// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! BILLING PARITY GATE, RELOCATED HERE from `busbar-core`'s `proxy/tests/billing_parity_tests.rs`
//! It drives the witnessed `StreamTranslate` and the dialect
//! readers directly to assert the IR-derived usage (`translate.usage()` / `reader().read_response()`)
//! produces EXACTLY the billed (input, output) tokens for every {streaming, non-stream} ×
//! {same-proto, cross-proto} path — so it names the codec vocabulary a neutral crate's tests must
//! not, and lives beside the types it exercises. Every assertion is BYTE-IDENTICAL to the
//! pre-relocation suite; only the paths were repointed (`crate::codec::proto::{StreamTranslate,
//! protocol_for,SSE_DONE_FRAME}` → the `super::*` prelude; `crate::codec::eventstream`/`crate::codec::json` are
//! this plane's own modules again since #83a SD-3).

use super::*;

/// Drive a CROSS-PROTOCOL streaming translator (egress → ingress) and return the IR A-tap tokens.
fn cross_proto_usage(ingress: &str, egress: &str, frames: &[&[u8]]) -> (u64, u64) {
    let mut t = StreamTranslate::new(ingress, egress).expect("cross-proto translator");
    for f in frames {
        let _ = t.feed(crate::codec::test_host::HOST, f);
    }
    let _ = t.finish(crate::codec::test_host::HOST);
    let u = t.usage().expect("A-tap captured terminal usage").clone();
    (u.input_tokens, u.output_tokens)
}

/// Decode a NON-STREAM body through `proto`'s reader (the same-proto non-stream billing path #4:
/// the body is relayed verbatim, billing reads `ir.usage`) and return the billed (input, output).
fn nonstream_usage(proto: &str, body: &[u8]) -> (u64, u64) {
    let p = protocol_for(proto).expect("known proto");
    let v: serde_json::Value = crate::codec::json::parse(body).expect("json body");
    let ir = p
        .reader()
        .read_response(crate::codec::test_host::HOST, &v)
        .expect("read_response");
    (ir.usage.input_tokens, ir.usage.output_tokens)
}

// ---- STREAMING × CROSS-PROTO (billing via translate.usage()) ----

#[test]
fn stream_cross_proto_anthropic_egress_to_openai_ingress() {
    // Cross-proto streaming still bills via the A-tap. Anthropic egress → OpenAI ingress: the
    // start-usage input backfill survives the seam, so billing sees the full (11, 7).
    assert_eq!(
            cross_proto_usage(
                "openai",
                "anthropic",
                &[
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
                    b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
                    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ],
            ),
            (11, 7),
        );
}

// ---- NON-STREAM × SAME-PROTO (billing path #4: read_response → ir.usage) ----

#[test]
fn nonstream_same_proto_all_protocols() {
    // The NEW non-stream same-proto billing path (#4): the body relays verbatim, billing reads the
    // egress reader's `ir.usage`. Each asserts the prior UsageTap (input, output).
    assert_eq!(
            nonstream_usage(
                "openai",
                br#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":13,"completion_tokens":9}}"#,
            ),
            (13, 9),
        );
    assert_eq!(
            nonstream_usage(
                "anthropic",
                br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}}"#,
            ),
            (11, 7),
        );
    assert_eq!(
            nonstream_usage(
                "gemini",
                br#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":4,"totalTokenCount":19}}"#,
            ),
            (15, 4),
        );
    assert_eq!(
            nonstream_usage(
                "responses",
                br#"{"id":"resp_1","object":"response","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":21,"output_tokens":8}}"#,
            ),
            (21, 8),
        );
}

// ---- ONE PROJECTION: the buffered billing arm and the stream tap must agree ----

/// A Cohere completion whose separately-metered `billed_units` differ from the raw `tokens` bucket
/// must be ledgered IDENTICALLY whether it was served buffered or streamed, and on the raw counts
/// 1.5.5 billed. The buffered arm is
/// `ChatRespHandle::billing()`; the streamed arm is the A-tap, which ledgers
/// `IrUsage::to_token_usage()`. Both answer the SAME completion, so a divergence here is a
/// same-request double answer: the operator is invoiced two different numbers depending on a
/// `stream` flag the provider's own counts do not depend on. Guards against the buffered arm
/// growing a SECOND projection.
#[test]
fn buffered_billing_and_stream_tap_agree_on_cohere_billed_units() {
    use busbar_contract::billing::Billing;
    use busbar_contract::ir::handle::IrHandle;

    // billed_units (120/50) deliberately DIFFER from the raw tokens bucket (100/40) — the only
    // shape that can tell the two projections apart.
    let body = br#"{"id":"c1","finish_reason":"COMPLETE","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"usage":{"billed_units":{"input_tokens":120,"output_tokens":50},"tokens":{"input_tokens":100,"output_tokens":40}}}"#;
    let v: serde_json::Value = crate::codec::json::parse(body).expect("json body");
    let ir = protocol_for("cohere")
        .expect("known proto")
        .reader()
        .read_response(crate::codec::test_host::HOST, &v)
        .expect("read_response");

    // The STREAM tap's ledgered projection of this exact IR.
    let streamed = ir.usage.to_token_usage();
    assert_eq!(
        (streamed.input, streamed.output),
        (100, 40),
        "the stream tap ledgers the RAW counts, as 1.5.5 did"
    );

    // The BUFFERED billing arm over the same IR.
    let buffered = match chat_handle::ChatRespHandle(
        ir,
        busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
    )
    .billing()
    {
        Some(Billing::Tokens(t)) => t,
        other => panic!("buffered chat billing must be token-shaped, got {other:?}"),
    };
    assert_eq!(
        buffered, streamed,
        "buffered and streamed billing must be ONE projection of one completion's usage"
    );
}
