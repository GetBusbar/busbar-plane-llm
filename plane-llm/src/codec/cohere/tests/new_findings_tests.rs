use super::*;
use crate::codec::ir::{IrBlock, IrMessage, IrRequest, IrRole};
use serde_json::json;

#[test]
fn malformed_embedding_buckets_are_not_empty_successes() {
    for value in [
        json!({}),
        json!({"embeddings": null}),
        json!({"embeddings": {}}),
        json!({"embeddings": {"float": "invalid"}}),
        json!({"embeddings": {"float": [[1.0]], "int8": null}}),
    ] {
        assert!(handler::read_embeddings_response(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    assert!(handler::read_embeddings_response(br#"{"embeddings":{"float":[]}}"#).is_ok());
    assert_eq!(
        handler::read_embeddings_response(br#"{"embeddings":{"float":[[1.0]]}}"#)
            .unwrap()
            .embeddings
            .len(),
        1
    );
}

#[test]
fn malformed_chat_messages_retain_independent_usage() {
    for message in [json!(null), json!("text"), json!([]), json!(false)] {
        let body =
            json!({"message":message,"usage":{"tokens":{"input_tokens":7,"output_tokens":3}}});
        assert!(CohereReader
            .read_response(crate::codec::test_host::HOST, &body)
            .is_err());
        let usage = CohereReader.recover_response_usage(&body).unwrap();
        assert_eq!(usage.input, 7);
        assert_eq!(usage.output, 3);
    }
    assert!(CohereReader
        .read_response(crate::codec::test_host::HOST, &json!({"message":{}}))
        .is_ok());
}

#[test]
fn many_tool_prefix_blocks_keep_order_and_are_folded_once() {
    let prefix: Vec<_> = (0..4000).map(|n| format!("p{n}")).collect();
    let mut content: Vec<_> = prefix.iter().cloned().map(IrBlock::text).collect();
    for id in ["first", "second"] {
        content.push(IrBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![IrBlock::text(id)],
            is_error: false,
            cache_control: None,
        });
    }
    let request = IrRequest {
        messages: vec![IrMessage {
            role: IrRole::Tool,
            content,
        }],
        ..Default::default()
    };
    let wire = CohereWriter.write_request(crate::codec::test_host::HOST, &request);
    let messages = wire["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], format!("{}first", prefix.join("")));
    assert_eq!(messages[1]["content"], "second");
}

#[test]
fn citation_anchor_lookup_handles_sparse_indices_and_interleaving() {
    let writer = CohereWriter;
    for n in 0..4000 {
        writer.record_citation_text(n * 1000, "é");
    }
    let stream = writer.stream.lock();
    assert_eq!(stream.citation_blocks.len(), 4000);
    assert_eq!(stream.citation_blocks[&3_999_000].base, 3999);
    drop(stream);
    writer.record_citation_text(0, "later");
    assert!(!writer.stream.lock().citation_blocks[&0].contiguous);
    writer.stream.lock().translate_cap = 1;
    writer.record_citation_text(usize::MAX, "over cap");
    assert!(!writer
        .stream
        .lock()
        .citation_blocks
        .contains_key(&usize::MAX));
}
