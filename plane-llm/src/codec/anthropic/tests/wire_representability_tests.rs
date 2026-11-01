use super::*;
use crate::codec::ir::{IrBlock, IrCitation, IrRequest, IrResponse};
use crate::codec::test_host::HOST;
use serde_json::{json, Value};

fn text(citations: Vec<IrCitation>) -> IrBlock {
    IrBlock::Text {
        text: "Paris".into(),
        cache_control: None,
        citations,
        refusal: false,
    }
}

fn web() -> IrCitation {
    IrCitation {
        cited_text: Some("Paris".into()),
        title: Some("Atlas".into()),
        url: Some("https://atlas.example".into()),
        ..Default::default()
    }
}

fn tool(input: Value) -> IrBlock {
    IrBlock::ToolUse {
        id: "toolu_1".into(),
        name: "weather".into(),
        input,
        cache_control: None,
        thought_signature: None,
    }
}

#[test]
fn unavailable_citation_coordinates_drop_with_audit_and_preserve_text_and_usage() {
    let response = IrResponse {
        content: vec![text(vec![
            web(),
            IrCitation {
                kind: Some(CITATION_TYPE_CHAR.into()),
                cited_text: Some("Paris".into()),
                start_index: Some(0),
                end_index: Some(5),
                ..Default::default()
            },
        ])],
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
        || {
            anthropic_writer()
                .try_write_response(HOST, &response)
                .unwrap()
        },
    );
    assert_eq!(out["content"][0]["text"], "Paris");
    assert!(out["content"][0]["citations"].is_null());
    assert_eq!(out["usage"]["input_tokens"], 17);
    assert_eq!(out["usage"]["output_tokens"], 9);
    assert_eq!(dropped, vec!["citations"]);
}

#[test]
fn complete_document_citations_use_nullable_response_members_without_inventing_coordinates() {
    for kind in [
        CITATION_TYPE_CHAR,
        CITATION_TYPE_PAGE,
        CITATION_TYPE_CONTENT_BLOCK,
    ] {
        let citation = IrCitation {
            kind: Some(kind.into()),
            cited_text: Some("Paris".into()),
            document_index: Some(3),
            start_index: Some(1),
            end_index: Some(5),
            ..Default::default()
        };
        let request = write_citation(&citation).expect("complete citation");
        assert!(
            request.get("file_id").is_none(),
            "request schema has no file_id"
        );
        let response = write_response_citation(&citation).expect("complete citation");
        assert_eq!(response["document_index"], 3);
        assert_eq!(response.get("document_title"), Some(&Value::Null));
        assert_eq!(response.get("file_id"), Some(&Value::Null));
        let incomplete = IrCitation {
            document_index: None,
            ..citation
        };
        assert!(write_response_citation(&incomplete).is_none());
    }
}

#[test]
fn a_retained_native_opaque_index_survives_without_a_synthetic_replacement() {
    let raw = json!({"type":"web_search_result_location", "cited_text":"Paris",
        "url":"https://atlas.example", "title":null, "encrypted_index":"opaque-native-index"});
    assert_eq!(write_response_citation(&read_citation(&raw)), Some(raw));
    let mut citation = web();
    citation.title = None;
    citation.encrypted_index = Some("opaque-native-index".into());
    let written = write_response_citation(&citation).unwrap();
    assert_eq!(written["encrypted_index"], "opaque-native-index");
    assert_eq!(written.get("title"), Some(&Value::Null));
}

#[test]
fn request_citation_constraints_do_not_restrict_valid_response_citations() {
    for (kind, title, url) in [
        (CITATION_TYPE_CHAR, String::new(), "https://atlas.example"),
        (CITATION_TYPE_PAGE, "x".repeat(501), "https://atlas.example"),
        (
            CITATION_TYPE_CONTENT_BLOCK,
            "x".repeat(501),
            "https://atlas.example",
        ),
        (
            CITATION_TYPE_WEB_SEARCH,
            String::new(),
            "https://atlas.example",
        ),
        (CITATION_TYPE_WEB_SEARCH, "Atlas".into(), ""),
    ] {
        let citation = IrCitation {
            kind: Some(kind.into()),
            title: Some(title),
            url: Some(url.into()),
            cited_text: Some("Paris".into()),
            document_index: Some(0),
            start_index: Some(1),
            end_index: Some(2),
            encrypted_index: Some("retained-native-index".into()),
            ..Default::default()
        };
        assert!(write_citation(&citation).is_none());
        assert!(write_response_citation(&citation).is_some());
    }
    let mut citation = web();
    citation.title = Some("é".repeat(512));
    citation.encrypted_index = Some("retained-native-index".into());
    assert!(
        write_citation(&citation).is_some(),
        "schema length counts Unicode characters"
    );
    citation.title = Some("é".repeat(513));
    assert!(write_citation(&citation).is_none());
    assert!(write_response_citation(&citation).is_none());
}

#[test]
fn unavailable_streamed_citation_emits_no_invalid_delta_and_text_continues() {
    let writer = anthropic_writer();
    let citation = IrStreamEvent::BlockDelta {
        index: 0,
        delta: IrDelta::CitationsDelta(vec![web()]),
    };
    assert!(writer
        .try_write_response_events(HOST, &citation)
        .unwrap()
        .is_empty());
    let text = IrStreamEvent::BlockDelta {
        index: 0,
        delta: IrDelta::TextDelta("Paris".into()),
    };
    let frames = writer.try_write_response_events(HOST, &text).unwrap();
    assert_eq!(frames[0].1["delta"]["text"], "Paris");
}

#[test]
fn non_object_tool_input_refuses_whole_response_and_nested_request_history() {
    for input in [
        json!("not json"),
        json!(null),
        json!([]),
        json!(7),
        json!(true),
    ] {
        let response = IrResponse {
            content: vec![tool(input.clone())],
            ..Default::default()
        };
        let writer = anthropic_writer();
        assert!(writer.try_write_response(HOST, &response).is_err());
        let error = writer.write_response(HOST, &response);
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "api_error");
        assert!(error.get("content").is_none());
        let request = IrRequest {
            system: vec![IrBlock::ToolResult {
                tool_use_id: "toolu_parent".into(),
                content: vec![tool(input)],
                is_error: false,
                cache_control: None,
            }],
            ..Default::default()
        };
        assert!(writer
            .try_write_request_for_lane(HOST, &request, "m", &LaneCaps::default())
            .is_err());
        assert_eq!(
            writer.write_request(HOST, &request)["error"]["type"],
            "invalid_request_error"
        );
    }
    let response = IrResponse {
        content: vec![tool(json!({"city":"Paris"}))],
        ..Default::default()
    };
    assert_eq!(
        anthropic_writer()
            .try_write_response(HOST, &response)
            .unwrap()["content"][0]["input"],
        json!({"city":"Paris"})
    );
}

#[test]
fn refused_response_keeps_the_upstream_billable_usage() {
    use busbar_contract::codec::{OperationHandler, TranslatedResponse};
    let body = json!({"id":"chatcmpl_1", "model":"m", "choices":[{"index":0,
        "message":{"role":"assistant", "tool_calls":[{"id":"call_1", "type":"function",
            "function":{"name":"weather", "arguments":"not json"}}]}, "finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":17, "completion_tokens":9, "total_tokens":26}});
    let mut handle = crate::codec::chat_handle::ChatOperation("openai")
        .read_response_value(HOST, &body)
        .unwrap();
    let before = handle.billing().expect("reported usage");
    handle.prepare_for_ingress("anthropic", 0);
    assert!(matches!(
        handle.write_ingress_response(HOST, "anthropic", true),
        TranslatedResponse::Untranslatable
    ));
    assert_eq!(handle.billing(), Some(before));
}

#[test]
fn streamed_non_object_tool_input_refuses_and_continues_metering() {
    let mut stream =
        crate::codec::proto_stream::StreamTranslate::new("anthropic", "openai").unwrap();
    let first = b"data: {\"id\":\"chatcmpl_1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"weather\",\"arguments\":\"not json\"}}]},\"finish_reason\":null}]}\n\n";
    let out = String::from_utf8(stream.feed(HOST, first)).unwrap();
    assert!(out.contains("event: error"), "{out}");
    assert!(!out.contains("\"partial_json\":\"not json\""), "{out}");
    assert!(stream.local_encoding_failure());
    let usage = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":9,\"total_tokens\":26}}\n\ndata: [DONE]\n\n";
    assert!(stream.feed(HOST, usage).is_empty());
    assert!(stream.finish(HOST).is_empty());
    let usage = stream.usage().expect("metering survives the local refusal");
    assert_eq!((usage.input_tokens, usage.output_tokens), (17, 9));
}

#[test]
fn buffered_stream_synthesis_cannot_serialize_a_string_tool_input() {
    let response = IrResponse {
        content: vec![tool(json!("not json"))],
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
fn object_arguments_can_arrive_in_fragments_after_whitespace() {
    let writer = anthropic_writer();
    for fragment in [" \n", "{", "\"city\":", "\"Paris\"}"] {
        let event = IrStreamEvent::BlockDelta {
            index: 0,
            delta: IrDelta::InputJsonDelta(fragment.into()),
        };
        let frames = writer.try_write_response_events(HOST, &event).unwrap();
        assert_eq!(frames[0].1["delta"]["partial_json"], fragment);
    }
}
