use super::*;
use crate::codec::ir::{IrBlock, IrCitation, IrRequest, IrResponse};
use crate::codec::test_host::HOST;
use serde_json::json;

fn malformed_tool() -> IrBlock {
    IrBlock::ToolUse {
        id: "toolu_1".into(),
        name: "weather".into(),
        input: json!("not json"),
        cache_control: None,
        thought_signature: None,
    }
}

#[test]
fn incomplete_foreign_citation_drops_without_losing_text_or_usage() {
    let response = IrResponse {
        content: vec![IrBlock::Text {
            text: "Paris".into(),
            cache_control: None,
            citations: vec![IrCitation {
                cited_text: Some("Paris".into()),
                title: Some("Atlas".into()),
                url: Some("https://atlas.example".into()),
                ..Default::default()
            }],
            refusal: false,
        }],
        usage: IrUsage {
            input_tokens: 17,
            output_tokens: 9,
            ..Default::default()
        },
        ..Default::default()
    };
    let (out, dropped) = crate::codec::drops::scope(
        crate::codec::drops::Seam {
            direction: crate::codec::drops::Direction::Response,
            ingress: "anthropic",
            egress: "gemini",
        },
        || anthropic_writer().write_response(HOST, &response),
    );
    assert_eq!(out["content"][0]["text"], "Paris");
    assert_eq!(out["usage"]["input_tokens"], 17);
    assert_eq!(out["usage"]["output_tokens"], 9);
    assert!(out["content"][0]["citations"].is_null(), "{out}");
    assert_eq!(dropped, vec!["citations"]);
}

#[test]
fn neutral_document_citation_has_required_response_nullable_members() {
    let block = IrBlock::Text {
        text: "Paris".into(),
        cache_control: None,
        refusal: false,
        citations: vec![IrCitation {
            kind: Some("char_location".into()),
            cited_text: Some("Paris".into()),
            document_index: Some(3),
            start_index: Some(0),
            end_index: Some(5),
            ..Default::default()
        }],
    };
    let response = IrResponse {
        content: vec![block],
        ..Default::default()
    };
    let out = anthropic_writer().write_response(HOST, &response);
    let citation = &out["content"][0]["citations"][0];
    assert_eq!(citation["document_index"], 3);
    assert_eq!(
        citation.get("document_title"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(citation.get("file_id"), Some(&serde_json::Value::Null));
}

#[test]
fn non_object_tool_input_is_a_native_error_instead_of_invalid_anthropic_content() {
    let response = IrResponse {
        content: vec![malformed_tool()],
        ..Default::default()
    };
    let out = anthropic_writer().write_response(HOST, &response);
    assert_eq!(out["type"], "error", "{out}");
    assert_eq!(out["error"]["type"], "api_error");
    assert!(out.get("content").is_none());
}

#[test]
fn non_object_tool_input_is_refused_in_request_history() {
    let request = IrRequest {
        system: vec![malformed_tool()],
        ..Default::default()
    };
    let out = anthropic_writer().write_request(HOST, &request);
    assert_eq!(out["type"], "error", "{out}");
    assert_eq!(out["error"]["type"], "invalid_request_error");
    assert!(out.get("messages").is_none());
}

#[test]
fn streamed_non_object_tool_input_refuses_while_later_usage_still_accumulates() {
    let mut stream =
        crate::codec::proto_stream::StreamTranslate::new("anthropic", "openai").unwrap();
    let first = b"data: {\"id\":\"chatcmpl_1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"weather\",\"arguments\":\"not json\"}}]},\"finish_reason\":null}]}\n\n";
    let out = String::from_utf8(stream.feed(HOST, first)).unwrap();
    assert!(out.contains("event: error"), "{out}");
    assert!(!out.contains("input_json_delta"), "{out}");
    let usage = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":9,\"total_tokens\":26}}\n\ndata: [DONE]\n\n";
    assert!(stream.feed(HOST, usage).is_empty());
    assert!(stream.finish(HOST).is_empty());
    let usage = stream.usage().expect("upstream usage remains billable");
    assert_eq!((usage.input_tokens, usage.output_tokens), (17, 9));
}

#[test]
fn buffered_stream_synthesis_refuses_non_object_tool_input() {
    let response = IrResponse {
        content: vec![malformed_tool()],
        ..Default::default()
    };
    let out = crate::codec::proto_stream::StreamTranslate::synthesize_from_response(
        HOST,
        "anthropic",
        &response,
        1024 * 1024,
    )
    .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("event: error"), "{out}");
    assert!(!out.contains("input_json_delta"), "{out}");
    assert!(!out.contains("event: message_stop"), "{out}");
}

#[test]
fn request_citation_constraints_leave_valid_response_citations_intact() {
    for (kind, title, url) in [
        ("char_location", String::new(), "https://atlas.example"),
        ("page_location", "x".repeat(501), "https://atlas.example"),
        (
            "content_block_location",
            "x".repeat(501),
            "https://atlas.example",
        ),
        (
            "web_search_result_location",
            String::new(),
            "https://atlas.example",
        ),
        ("web_search_result_location", "Atlas".into(), ""),
    ] {
        let block = IrBlock::Text {
            text: "Paris".into(),
            cache_control: None,
            refusal: false,
            citations: vec![IrCitation {
                kind: Some(kind.into()),
                title: Some(title),
                url: Some(url.into()),
                cited_text: Some("Paris".into()),
                document_index: Some(0),
                start_index: Some(1),
                end_index: Some(2),
                encrypted_index: Some("retained-native-index".into()),
                ..Default::default()
            }],
        };
        let request = IrRequest {
            system: vec![block.clone()],
            ..Default::default()
        };
        let request_wire = anthropic_writer().write_request(HOST, &request);
        assert!(
            request_wire["system"][0].get("citations").is_none(),
            "{request_wire}"
        );
        let response = IrResponse {
            content: vec![block],
            ..Default::default()
        };
        let response_wire = anthropic_writer().write_response(HOST, &response);
        assert!(
            response_wire["content"][0]["citations"][0].is_object(),
            "{response_wire}"
        );
    }
}
