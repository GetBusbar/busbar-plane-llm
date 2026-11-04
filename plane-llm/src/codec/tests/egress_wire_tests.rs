// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for the write-default's terminal: the contract's `IrHandle::write_egress_request` default
//! refuses rather than invents a body, and this plane's translate seam turns that refusal into the
//! 4xx reject. (Moved with the translate seam from `busbar-substrate-values`, SD-8.)

use crate::codec::egress_prep::EgressPrep;
use crate::codec::translate::{TranslateCodec, TranslateReqInput, TranslateReqReject};
use busbar_contract::codec::{CodecError, EgressWire, IngressReject, OperationHandler};
use busbar_contract::ir::handle::IrHandle;
use busbar_contract::ir::invoke::InvokeReq;
use busbar_contract::ir::neutral_handles::InvokeReqHandle;

fn prep() -> EgressPrep {
    EgressPrep {
        ingress_protocol: "a-dialect",
        egress_requires_max_tokens: false,
        lane_default_max_tokens: None,
        global_default_max_tokens: 0,
        reasoning_allowed: true,
        reasoning_budgets: [0; 4],
        prompt_caching_allowed: true,
        cache_control_cap: None,
        lane_caps: Default::default(),
        thought_signature_fill: false,
    }
}

fn a_request() -> InvokeReqHandle {
    InvokeReqHandle(std::sync::Arc::new(InvokeReq {
        tool: "search".to_string(),
        arguments: serde_json::json!({"q": "the caller's own words"}),
        extra: Default::default(),
    }))
}

/// A handle that does not override the JSON egress write cannot write itself onto ANY dialect, and
/// says so.
#[test]
fn the_write_default_is_unrepresentable_and_names_the_dialect() {
    let mut handle = a_request();
    match handle.write_egress_request(
        crate::codec::test_host::HOST,
        "some-other-dialect",
        "a-model",
    ) {
        EgressWire::Unrepresentable { reason } => assert!(
            reason.contains("some-other-dialect"),
            "the refusal must name the dialect it could not be written onto, got: {reason}"
        ),
        EgressWire::Json(v) => panic!("the write default must not invent a body, got {v}"),
        EgressWire::Bytes(b) => panic!(
            "the write default must not answer with a body, got {} bytes",
            b.len()
        ),
    }
}

/// A codec whose read yields a handle that keeps the write default — the shape the translate seam
/// must refuse rather than forward.
struct DefaultWriteCodec;

impl OperationHandler for DefaultWriteCodec {
    fn read_request_value(
        &self,
        _v: &serde_json::Value,
    ) -> Result<Box<dyn IrHandle>, IngressReject> {
        Ok(Box::new(a_request()))
    }
    fn read_request(
        &self,
        _body: &[u8],
        _content_type: &str,
    ) -> Result<Box<dyn IrHandle>, IngressReject> {
        Ok(Box::new(a_request()))
    }
    fn read_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _wire: &[u8],
    ) -> Result<Box<dyn IrHandle>, CodecError> {
        Err(CodecError::Malformed("not a response codec".to_string()))
    }
}

/// The seam turns the unrepresentable write into the SAME refusal the pre-write representability
/// guard raises — a 4xx carrying the reason — instead of handing the router an empty body to send.
#[test]
fn the_translate_seam_refuses_an_unrepresentable_write() {
    let body = serde_json::json!({"tool": "search"});
    let reject = DefaultWriteCodec
        .translate_request(
            crate::codec::test_host::HOST,
            TranslateReqInput::Json(&body),
            Some("some-other-dialect"),
            &prep(),
            "a-model",
        )
        .err()
        .expect("a handle that cannot write itself must not yield an egress wire");
    match reject {
        TranslateReqReject::Unrepresentable(reason) => assert!(
            reason.contains("some-other-dialect"),
            "the reject carries the writer's own reason, got: {reason}"
        ),
        TranslateReqReject::Ingress(_) => panic!("the body parsed; this is not an ingress reject"),
        TranslateReqReject::EgressUnsupported => {
            panic!("the egress dialect was supplied; this is not the 404 terminal")
        }
    }
}

#[test]
fn chat_preparation_rejects_foreign_context_before_changing_the_request() {
    use crate::codec::chat_handle::ChatReqHandle;
    use crate::codec::egress_prep::LaneCaps;
    use crate::codec::proto_codec::ProtocolReader;

    let body = serde_json::json!({
        "model": "m", "messages": [{"role": "user", "content": "hello"}],
        "reasoning_effort": "high", "max_tokens": 32000
    });
    let request = crate::codec::openai_chat::OpenAiReader
        .read_request(&body)
        .expect("valid chat request");
    let mut handle = ChatReqHandle(request, LaneCaps::NONE);
    let before = handle.write_egress_request(crate::codec::test_host::HOST, "openai", "m");
    let error = handle
        .prepare_for_egress(&42_u32)
        .expect_err("an unrelated Rust context must fail closed");
    let CodecError::Malformed(reason) = error;
    assert!(reason.contains("EgressPrep"));
    assert_eq!(handle.1, LaneCaps::NONE);
    let after = handle.write_egress_request(crate::codec::test_host::HOST, "openai", "m");
    match (before, after) {
        (EgressWire::Json(before), EgressWire::Json(after)) => assert_eq!(before, after),
        _ => panic!("the real chat writer must produce JSON"),
    }
}

fn prepared_chat_wire(
    ingress: &'static str,
    egress: &str,
    body: &serde_json::Value,
    caps: crate::codec::egress_prep::LaneCaps,
) -> serde_json::Value {
    let mut context = prep();
    context.ingress_protocol = ingress;
    context.lane_caps = caps;
    context.reasoning_budgets = crate::codec::ir::REASONING_BUDGET_DEFAULTS;
    let translated = crate::codec::chat_handle::ChatOperation(ingress)
        .translate_request(
            crate::codec::test_host::HOST,
            TranslateReqInput::Json(body),
            Some(egress),
            &context,
            "m",
        )
        .unwrap_or_else(|_| panic!("registered chat dialects must translate"));
    match translated.wire {
        EgressWire::Json(value) => value,
        _ => panic!("the real chat writer must produce JSON"),
    }
}

#[test]
fn opaque_preparation_preserves_anthropic_adaptive_and_budget_wire_forms() {
    use crate::codec::egress_prep::LaneCaps;
    let body = serde_json::json!({
        "model": "m", "messages": [{"role": "user", "content": "hello"}],
        "reasoning_effort": "high", "max_tokens": 32000
    });
    let adaptive = prepared_chat_wire(
        "openai",
        "anthropic",
        &body,
        LaneCaps {
            anthropic_adaptive_thinking: true,
            ..LaneCaps::NONE
        },
    );
    assert_eq!(
        adaptive["thinking"],
        serde_json::json!({"type": "adaptive"})
    );
    assert_eq!(adaptive["output_config"]["effort"], "high");
    let budget = prepared_chat_wire("openai", "anthropic", &body, LaneCaps::NONE);
    assert_eq!(
        budget["thinking"],
        serde_json::json!({
            "type": "enabled",
            "budget_tokens": crate::codec::ir::REASONING_BUDGET_DEFAULTS[3]
        })
    );
    assert!(budget.get("output_config").is_none());
}

#[test]
fn opaque_preparation_preserves_openai_output_token_key_wire_forms() {
    use crate::codec::egress_prep::{LaneCaps, MaxOutputKey};
    let body = serde_json::json!({
        "model": "m", "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 100
    });
    let completion = prepared_chat_wire(
        "anthropic",
        "openai",
        &body,
        LaneCaps {
            max_output_key: MaxOutputKey::MaxCompletionTokens,
            ..LaneCaps::NONE
        },
    );
    assert_eq!(completion["max_completion_tokens"], 100);
    assert!(completion.get("max_tokens").is_none());
    let legacy = prepared_chat_wire("anthropic", "openai", &body, LaneCaps::NONE);
    assert_eq!(legacy["max_tokens"], 100);
    assert!(legacy.get("max_completion_tokens").is_none());
}
