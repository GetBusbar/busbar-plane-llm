use super::*;
use serde_json::json;

#[test]
fn native_request_origins_follow_system_fold_and_nested_result() {
    let body = json!({
        "system": [{"type": "text", "text": "top"}],
        "messages": [
            {"role": "system", "content": [{"type": "thinking", "thinking": "folded"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "image", "source": {"type": "url", "url": "https://example.test/i"}}
                ]},
                {"type": "redacted_thinking", "data": "opaque"}
            ]}
        ]
    });
    let req = AnthropicReader.read_request(&body).expect("native request");
    assert_eq!(req.system.len(), req.block_origins.system.len());
    assert_eq!(req.messages.len(), req.block_origins.messages.len());
    assert_eq!(
        req.messages[0].content.len(),
        req.block_origins.messages[0].len()
    );
    assert_eq!(
        req.block_origins.system[1].source.unwrap().path,
        "messages[].content[].type=thinking"
    );
    let result = &req.block_origins.messages[0][0];
    assert_eq!(
        result.source.unwrap().path,
        "messages[].content[].type=tool_result"
    );
    assert_eq!(result.children.len(), 2);
    assert_eq!(
        result.children[0].source.unwrap().path,
        "messages[].content[].type=tool_result.content[]"
    );
    assert_eq!(
        req.block_origins.messages[0][1].source.unwrap().path,
        "messages[].content[].type=redacted_thinking"
    );
}

#[test]
fn native_response_origins_follow_filtered_blocks() {
    let body = json!({
        "role": "assistant", "content": [
            {"type": "text", "text": "answer"},
            {"type": "unknown", "future": true},
            {"type": "redacted_thinking", "data": "opaque"}
        ]
    });
    let resp = AnthropicReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("native response");
    assert_eq!(resp.content.len(), resp.block_origins.response.len());
    assert_eq!(
        resp.block_origins.response[1].source.unwrap().path,
        "content[].type=redacted_thinking"
    );
}
