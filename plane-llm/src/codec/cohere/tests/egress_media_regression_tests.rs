// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Regression net: a top-level `IrBlock::Media` (document/audio/video) on Cohere egress must be
//! dropped WITH the standard drop-with-warn `warn!`, never silently — the IR Media contract.

use super::*;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

#[test]
fn anthropic_to_cohere_document_media_warns_not_silent() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![
                crate::codec::ir::IrBlock::text("summarize this".to_string()),
                crate::codec::ir::IrBlock::Media {
                    kind: crate::codec::ir::IrMediaKind::Document,
                    source: crate::codec::ir::IrImageSource::Base64 {
                        media_type: "application/pdf".to_string(),
                        data: "JVBERi0=".to_string(),
                    },
                    name: Some("report.pdf".to_string()),
                    cache_control: None,
                    citations: None,
                    context: None,
                },
            ],
        }],
        ..Default::default()
    };

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let out = tracing::dispatcher::with_default(&subscriber, || {
        let w = CohereWriter;
        w.write_request(crate::codec::test_host::HOST, &ir)
    });

    let msgs = out["messages"].as_array().expect("messages array");
    let user = msgs.last().expect("a user message");
    let dump = serde_json::to_string(user).unwrap();
    assert!(
        !dump.contains("application/pdf") && !dump.contains("JVBERi0="),
        "the document Media must not leak onto the Cohere message content: {dump}"
    );

    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("dropping attachment on Cohere egress")),
        "a dropped top-level Media block must warn (drop-with-warn, never silently): {:?}",
        diagnostic_texts(&cap)
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|m| m.contains("media_kind=document")),
        "the warn must name the dropped media kind: {:?}",
        diagnostic_texts(&cap)
    );
}
