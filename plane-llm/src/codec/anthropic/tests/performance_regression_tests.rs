use super::*;
use crate::codec::ir::{IrBlock, IrImageSource, IrMediaKind};
use serde_json::json;

#[test]
fn audit_anthropic_stash_index_preserves_duplicates_and_invalid_entries() {
    let entries = vec![
        json!({"m": 1, "i": 3}),
        json!({"m": 1, "i": 3, "block": "first"}),
        json!({"m": 1, "i": 3, "block": "second"}),
        json!({"m": 1, "i": 2, "block": "a", "insert": true}),
        json!({"m": 1, "i": 0, "block": "b", "insert": true}),
        json!({"m": 1, "i": 2, "block": "c", "insert": true}),
        json!({"m": -1, "i": 3, "block": "bad"}),
    ];
    let indexed = index_stash(&entries);
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[&1].replacements[&3], &json!("first"));
    assert_eq!(
        indexed[&1]
            .inserts
            .iter()
            .map(|(i, b)| (*i, (*b).clone()))
            .collect::<Vec<_>>(),
        vec![(0, json!("b")), (2, json!("a")), (2, json!("c"))]
    );
    assert!(!indexed.contains_key(&0));
}

#[test]
fn audit_anthropic_stash_index_borrows_payloads_at_scale() {
    for n in [32, 1024] {
        let entries: Vec<_> = (0..n)
            .map(|m| json!({"m": m, "i": 0, "block": {"text": "large".repeat(1024)}}))
            .collect();
        let indexed = index_stash(&entries);
        assert_eq!(indexed.len(), n);
        for m in 0..n {
            assert!(std::ptr::eq(
                indexed[&m].replacements[&0],
                &entries[m]["block"]
            ));
        }
    }
}

fn document(data: String) -> IrBlock {
    IrBlock::Media {
        kind: IrMediaKind::Document,
        source: IrImageSource::Base64 {
            media_type: "text/plain".into(),
            data,
        },
        name: None,
        cache_control: None,
        citations: None,
        context: None,
    }
}

#[test]
fn audit_anthropic_nested_document_prepared_once_and_invalid_utf8_dropped() {
    let text = "a".repeat(1024 * 1024);
    let data = busbar_contract::media::base64_encode(text.as_bytes());
    let block = IrBlock::ToolResult {
        tool_use_id: "tool".into(),
        content: vec![document(data), document("/w==".into())],
        is_error: false,
        cache_control: None,
    };
    DOCUMENT_PREPARATIONS.with(|count| count.set(0));
    let output = write_block(&block);
    assert_eq!(output["content"].as_array().unwrap().len(), 1);
    assert_eq!(output["content"][0]["source"]["data"], text);
    DOCUMENT_PREPARATIONS.with(|count| assert_eq!(count.get(), 2));
}

#[test]
fn audit_anthropic_document_preparation_reused_and_native_custody_preserved() {
    let msg = crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::User,
        content: vec![document("aGVsbG8=".into())],
    };
    DOCUMENT_PREPARATIONS.with(|count| count.set(0));
    let output = write_message(&msg, None, &[]);
    assert_eq!(output["content"][0]["source"]["data"], "hello");
    DOCUMENT_PREPARATIONS.with(|count| assert_eq!(count.get(), 1));
    let original = json!({"type": "document", "source": {"type": "text", "data": "hello"}, "citations": {"enabled": true}});
    let entries = vec![
        json!({"m": 0, "i": 0, "block": {"type": "native-before"}, "insert": true}),
        json!({"m": 0, "i": 1, "block": original}),
        json!({"m": 0, "i": 99, "block": {"type": "native-after"}, "insert": true}),
    ];
    let indexed = index_stash(&entries);
    let output = write_message(&msg, indexed.get(&0), &[]);
    assert_eq!(
        output["content"],
        json!([{"type": "native-before"}, original, {"type": "native-after"}])
    );
}
