// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use serde_json::{json, Value};

fn tool_request(content: Value) -> Value {
    json!({"messages": [
        {"role": "assistant", "content": [{"toolUse": {
            "toolUseId": "t1", "name": "fetch", "input": {}
        }}]},
        {"role": "user", "content": [{"toolResult": {
            "toolUseId": "t1", "status": "success", "content": content
        }}]}
    ]})
}

fn roundtrip(body: &Value) -> Value {
    let ir = BedrockReader.read_request(body).expect("Bedrock request");
    let writer = BedrockWriter;
    writer.write_request(crate::codec::test_host::HOST, &ir)
}

fn s3_source() -> Value {
    json!({"s3Location": {
        "uri": "s3://example-bucket/result", "bucketOwner": "123456789012"
    }})
}

#[test]
fn tool_result_s3_csv_preserves_format_and_source() {
    let content = json!([{"document": {
        "format": "csv", "name": "report", "source": s3_source(),
        "citations": {"enabled": true}, "context": "quarterly results"
    }}]);
    let out = roundtrip(&tool_request(content.clone()));
    assert_eq!(
        out["messages"][1]["content"][0]["toolResult"]["content"],
        content
    );
}

#[test]
fn tool_result_s3_webm_preserves_format_and_source() {
    let content = json!([{"video": {"format": "webm", "source": s3_source()}}]);
    let out = roundtrip(&tool_request(content.clone()));
    assert_eq!(
        out["messages"][1]["content"][0]["toolResult"]["content"],
        content
    );
}

#[test]
fn top_level_s3_media_stays_lossless_without_duplicate_blocks() {
    let content = json!([
        {"text": "Summarize these results"},
        {"document": {"format": "csv", "name": "report", "source": s3_source()}},
        {"video": {"format": "webm", "source": s3_source()}}
    ]);
    let out = roundtrip(&json!({"messages": [{"role": "user", "content": content}]}));
    assert_eq!(out["messages"][0]["content"], content);
}

#[test]
fn tool_result_inline_csv_and_webm_keep_their_bytes_and_formats() {
    let content = json!([
        {"document": {"format": "csv", "name": "report", "source": {"bytes": "YSxiCg=="}}},
        {"video": {"format": "webm", "source": {"bytes": "GkXfow=="}}}
    ]);
    let out = roundtrip(&tool_request(content.clone()));
    assert_eq!(
        out["messages"][1]["content"][0]["toolResult"]["content"],
        content
    );
}

#[test]
fn legacy_source_only_vendor_media_keeps_existing_defaults() {
    let content = json!([
        {"document": {"format": "pdf", "name": "report", "source": s3_source()}},
        {"video": {"format": "mp4", "source": s3_source()}}
    ]);
    let mut ir = BedrockReader
        .read_request(&tool_request(content.clone()))
        .expect("request");
    let crate::codec::ir::IrBlock::ToolResult {
        content: blocks, ..
    } = &mut ir.messages[1].content[0]
    else {
        panic!("tool result");
    };
    for block in blocks {
        let crate::codec::ir::IrBlock::Media { source, .. } = block else {
            panic!("media attachment");
        };
        *source = crate::codec::ir::IrImageSource::Vendor {
            vendor: VENDOR_NAME,
            value: s3_source(),
        };
    }
    let writer = BedrockWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["messages"][1]["content"][0]["toolResult"]["content"],
        content
    );
}
