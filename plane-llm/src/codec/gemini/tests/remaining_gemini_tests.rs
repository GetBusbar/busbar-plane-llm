// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::super::*;
use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStreamEvent, StreamDecodeState};
use crate::codec::proto_codec::protocol_for;
use serde_json::{json, Value};

fn tool_start(index: usize, name: &str) -> IrStreamEvent {
    IrStreamEvent::BlockStart {
        index,
        block: IrBlockMeta::ToolUse {
            id: format!("call-{index}"),
            name: name.to_string(),
        },
        refusal: false,
    }
}

fn argument(writer: &GeminiWriter, index: usize, text: &str) {
    assert!(writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::InputJsonDelta(text.to_string()),
            }
        )
        .is_none());
}

#[test]
fn an_overflowing_fragment_cannot_be_removed_to_make_other_arguments_runnable() {
    let writer = GeminiWriter;
    writer.set_translate_cap(16);
    let _ = writer.write_response_event(crate::codec::test_host::HOST, &tool_start(0, "send"));
    let _ = writer.write_response_event(crate::codec::test_host::HOST, &tool_start(1, "parallel"));
    argument(&writer, 0, "{\"x\":\"");
    argument(&writer, 0, &"must-not-disappear".repeat(2));
    // The old writer dropped only the long fragment, then resumed and emitted {"x":""}.
    argument(&writer, 0, "\"}");
    let (_, failed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("overflow is reported");
    assert_eq!(failed.pointer("/error/code"), Some(&json!(500)));
    assert_eq!(failed.pointer("/error/status"), Some(&json!("INTERNAL")));
    assert!(
        failed
            .pointer("/candidates/0/content/parts/0/functionCall")
            .is_none(),
        "{failed}"
    );
    let (_, parallel) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 1 },
        )
        .expect("an independent zero-argument tool survives");
    assert_eq!(
        parallel.pointer("/candidates/0/content/parts/0/functionCall/args"),
        Some(&json!({}))
    );
    let _ = writer.write_response_event(crate::codec::test_host::HOST, &tool_start(0, "new-call"));
    argument(&writer, 0, "{\"x\":1}");
    let (_, fresh) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 0 },
        )
        .expect("a new block has fresh state");
    assert_eq!(
        fresh.pointer("/candidates/0/content/parts/0/functionCall/args"),
        Some(&json!({"x":1}))
    );
}

#[test]
fn buffered_bedrock_image_output_reaches_the_gemini_client() {
    let source = json!({"output":{"message":{"role":"assistant","content":[
        {"text":"image"}, {"image":{"format":"png","source":{"bytes":"iVBORw0KGgo="}}}
    ]}},"stopReason":"end_turn","usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}});
    let response = protocol_for("bedrock")
        .unwrap()
        .reader()
        .read_response(crate::codec::test_host::HOST, &source)
        .expect("read real Converse output");
    let wire = GeminiWriter.write_response(crate::codec::test_host::HOST, &response);
    assert_eq!(
        wire.pointer("/candidates/0/content/parts/1/inlineData"),
        Some(&json!({"mimeType":"image/png","data":"iVBORw0KGgo="})),
        "{wire}"
    );
}

#[test]
fn buffered_generated_media_uses_the_same_native_part_as_streaming_media() {
    use crate::codec::ir::{IrImageSource, IrMediaKind, IrResponse};
    let media = IrBlock::Media {
        kind: IrMediaKind::Audio,
        source: IrImageSource::Base64 {
            media_type: "audio/wav".to_string(),
            data: "UklGRg==".to_string(),
        },
        name: None,
        cache_control: None,
        citations: None,
        context: None,
    };
    let response = IrResponse {
        content: vec![media.clone()],
        ..Default::default()
    };
    let writer = GeminiWriter;
    let buffered = writer.write_response(crate::codec::test_host::HOST, &response);
    let (_, streamed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: IrDelta::MediaDelta(Box::new(media)),
            },
        )
        .expect("media part");
    let expected = json!({"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}});
    assert_eq!(
        buffered.pointer("/candidates/0/content/parts/0"),
        Some(&expected)
    );
    assert_eq!(
        streamed.pointer("/candidates/0/content/parts/0"),
        Some(&expected)
    );
}

fn citations(events: &[IrStreamEvent]) -> Vec<crate::codec::ir::IrCitation> {
    events
        .iter()
        .filter_map(|event| match event {
            IrStreamEvent::BlockDelta {
                delta: IrDelta::CitationsDelta(citations),
                ..
            } => Some(citations.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn read_chunk(state: &mut StreamDecodeState, candidate: Value) -> Vec<IrStreamEvent> {
    GeminiReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"candidates":[candidate]}),
        state,
    )
}

#[test]
fn incremental_chunks_use_response_wide_indices_and_do_not_share_the_corpus_watermark() {
    let mut state = StreamDecodeState::default();
    let first_support =
        json!({"segment":{"startIndex":0,"endIndex":3,"text":"one"},"groundingChunkIndices":[0]});
    let first = read_chunk(
        &mut state,
        json!({"content":{"parts":[{"text":"one "}]},
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://source-a"}}],"groundingSupports":[first_support.clone()]}}),
    );
    let second = read_chunk(
        &mut state,
        json!({"content":{"parts":[{"text":"two"}]},
        "citationMetadata":{"citationSources":[{"uri":"https://corpus","startIndex":0,"endIndex":3}]},
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://source-b"}}],
            "groundingSupports":[{"segment":{"startIndex":4,"endIndex":7,"text":"two"},"groundingChunkIndices":[1]}]}}),
    );
    let mut found = citations(&first);
    found.extend(citations(&second));
    assert_eq!(
        found
            .iter()
            .filter_map(|c| c.url.as_deref())
            .collect::<Vec<_>>(),
        ["https://source-a", "https://corpus", "https://source-b"]
    );
    assert_eq!(
        (found[2].start_index, found[2].end_index),
        (Some(4), Some(7))
    );
    let replayed = read_chunk(
        &mut state,
        json!({"groundingMetadata":{"groundingSupports":[first_support]}}),
    );
    assert!(
        citations(&replayed).is_empty(),
        "a restated support is not a new citation"
    );
    let late = read_chunk(
        &mut state,
        json!({"groundingMetadata":{"groundingSupports":[
        {"segment":{"startIndex":1,"endIndex":3,"text":"ne"},"groundingChunkIndices":[0]}]}}),
    );
    let late = citations(&late);
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].url.as_deref(), Some("https://source-a"));
    assert_eq!((late[0].start_index, late[0].end_index), (Some(1), Some(3)));
}

fn two_parts() -> Value {
    json!({"content":{"role":"model","parts":[{"text":"AAAA"},{"text":"Bé中B"}]},
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://second-part"}}],
            "groundingSupports":[{"segment":{"partIndex":1,"startIndex":1,"endIndex":6,"text":"é中"},"groundingChunkIndices":[0]}]},
        "finishReason":"STOP"})
}

#[test]
fn buffered_grounding_offsets_are_utf8_bytes_inside_the_named_part() {
    let response = GeminiReader
        .read_response(
            crate::codec::test_host::HOST,
            &json!({"candidates":[two_parts()]}),
        )
        .expect("buffered answer");
    let texts: Vec<_> = response
        .content
        .iter()
        .filter_map(|block| match block {
            IrBlock::Text {
                text, citations, ..
            } => Some((text.as_str(), citations)),
            _ => None,
        })
        .collect();
    assert!(
        texts[0].1.is_empty(),
        "the first Part is not cited: {texts:?}"
    );
    assert_eq!(texts[1].0, "Bé中B");
    assert_eq!(texts[1].1.len(), 1);
    assert_eq!(
        (texts[1].1[0].start_index, texts[1].1[0].end_index),
        (Some(1), Some(3))
    );
    let foreign = protocol_for("openai")
        .unwrap()
        .writer()
        .write_response(crate::codec::test_host::HOST, &response);
    assert_eq!(
        foreign.pointer("/choices/0/message/annotations/0/url_citation/url"),
        Some(&json!("https://second-part")),
        "{foreign}"
    );
    assert_eq!(
        foreign.pointer("/choices/0/message/annotations/0/url_citation/start_index"),
        Some(&json!(5)),
        "{foreign}"
    );
    assert_eq!(
        foreign.pointer("/choices/0/message/annotations/0/url_citation/end_index"),
        Some(&json!(7)),
        "{foreign}"
    );
}

#[test]
fn streamed_grounding_uses_the_named_parts_utf8_coordinates() {
    let mut state = StreamDecodeState::default();
    let events = read_chunk(&mut state, two_parts());
    let found = citations(&events);
    assert_eq!(found.len(), 1);
    // These two adjacent wire Parts share one streamed IR Text block.
    assert_eq!(
        (found[0].start_index, found[0].end_index),
        (Some(5), Some(7))
    );
}

#[test]
fn a_part_fragment_start_belongs_to_the_next_fragment_after_interleaved_text() {
    let mut state = StreamDecodeState::default();
    let _ = read_chunk(
        &mut state,
        json!({"content":{"parts":[{"text":"A"},{"text":"B"}]}}),
    );
    let events = read_chunk(
        &mut state,
        json!({"content":{"parts":[{"text":"C"}]},
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://c"}}],
        "groundingSupports":[{"segment":{"partIndex":0,"startIndex":1,"endIndex":2,"text":"C"},"groundingChunkIndices":[0]}]}}),
    );
    let found = citations(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].start_index, found[0].end_index),
        (Some(2), Some(3))
    );
}

#[test]
fn oversized_grounding_does_not_discard_terminal_usage_from_the_actual_translator() {
    let frame = json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://source","title":"x".repeat(8 * 1024 * 1024)}}]},
        "finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":9,"totalTokenCount":16}});
    let wire = format!("data: {frame}\n\n");
    let mut translator = crate::codec::proto_stream::StreamTranslate::new("openai", "gemini")
        .expect("cross-protocol translator");
    let _ = translator.feed(crate::codec::test_host::HOST, wire.as_bytes());
    let _ = translator.finish(crate::codec::test_host::HOST);
    let usage = translator
        .usage()
        .expect("reported terminal usage survives the metadata failure");
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 9);
    assert!(
        translator.terminal_error().is_none(),
        "optional metadata is not an upstream failure"
    );
    assert!(
        translator
            .dropped()
            .iter()
            .any(|path| path == "candidates[].groundingMetadata"),
        "the metadata loss is audited: {:?}",
        translator.dropped()
    );
    let mut native = crate::codec::proto_stream::StreamTranslate::new_same_proto("gemini")
        .expect("native usage side channel");
    let mut native_wire = native.feed(crate::codec::test_host::HOST, wire.as_bytes());
    native_wire.extend(native.finish(crate::codec::test_host::HOST));
    assert_eq!(native_wire, wire.as_bytes(), "native output is unchanged");
    assert!(native.terminal_error().is_none());
    let usage = native
        .usage()
        .expect("native reported usage survives optional metadata loss");
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 9));
}

#[test]
fn actual_tool_overflow_fails_encoding_without_discarding_reported_usage() {
    let mut translator = crate::codec::proto_stream::StreamTranslate::new("gemini", "openai")
        .expect("cross-protocol translator")
        .with_translate_cap(16);
    let frames = [
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-overflow","type":"function","function":{"name":"send","arguments":"{\"x\":\""}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"must-not-disappear".repeat(2)}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":9,"total_tokens":16}}),
    ];
    let mut output = Vec::new();
    for frame in frames {
        output.extend(translator.feed(
            crate::codec::test_host::HOST,
            format!("data: {frame}\n\n").as_bytes(),
        ));
    }
    output.extend(translator.finish(crate::codec::test_host::HOST));
    assert!(translator.terminal_error().is_some(), "encoding must fail");
    let usage = translator
        .usage()
        .expect("upstream usage remains reportable");
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 9));
    let output = String::from_utf8(output).expect("SSE text");
    assert!(output.contains("INTERNAL"), "{output}");
    assert!(
        !output.contains("functionCall"),
        "no altered call reaches the client: {output}"
    );
    assert!(
        !output.contains("finishReason"),
        "no success follows the failure: {output}"
    );
}

fn schema_request(schema: Value) -> crate::codec::ir::IrRequest {
    protocol_for("openai")
        .unwrap()
        .reader()
        .read_request(&json!({
            "model": "test", "messages": [{"role": "user", "content": "hello"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "result", "schema": schema}},
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": schema}}]
        }))
        .expect("existing OpenAI schema request")
}

fn assert_schema_egress(schema: Value, expected: Value) {
    let mut request = schema_request(schema.clone());
    request.extra.clear();
    let wire = GeminiWriter.write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(
        wire.pointer("/generationConfig/responseSchema"),
        Some(&expected)
    );
    assert_eq!(
        wire.pointer("/tools/0/functionDeclarations/0/parameters"),
        Some(&expected)
    );
    assert_eq!(
        request.response_format.as_ref().unwrap().schema.as_ref(),
        Some(&schema)
    );
    assert_eq!(request.tools[0].input_schema, schema);
}

#[test]
fn nullable_schema_types_reach_native_response_and_tool_slots() {
    for (source, native) in [
        ("string", "STRING"),
        ("number", "NUMBER"),
        ("integer", "INTEGER"),
        ("boolean", "BOOLEAN"),
        ("array", "ARRAY"),
        ("object", "OBJECT"),
    ] {
        for types in [json!([source, "null"]), json!(["null", source])] {
            assert_schema_egress(
                json!({"type": types, "description": "optional value"}),
                json!({"type": native, "nullable": true, "description": "optional value"}),
            );
        }
    }
}

#[test]
fn nullable_schema_conversion_follows_nested_schemas_and_resolved_references() {
    let schema = json!({
        "type": "object", "required": ["type", "records"], "additionalProperties": false,
        "$defs": {"Entry": {"type": ["string", "null"]}},
        "properties": {
            "type": {"$ref": "#/$defs/Entry", "description": "keep sibling"},
            "records": {"type": ["array", "null"], "minItems": 1, "maxItems": 3,
                "items": {"type": ["object", "null"], "properties": {
                    "value": {"anyOf": [{"type": ["integer", "null"], "minimum": 0, "maximum": 8}, {"type": "string"}]}
                }}}
        }
    });
    assert_schema_egress(
        schema,
        json!({
            "type": "object", "required": ["type", "records"], "additionalProperties": false,
            "properties": {
            "type": {"type": "STRING", "nullable": true, "description": "keep sibling"},
                "records": {"type": "ARRAY", "nullable": true, "minItems": 1, "maxItems": 3,
                    "items": {"type": "OBJECT", "nullable": true, "properties": {
                        "value": {"anyOf": [{"type": "INTEGER", "nullable": true, "minimum": 0, "maximum": 8}, {"type": "string"}]}
                    }}}
            }
        }),
    );
}

#[test]
fn schema_conversion_preserves_native_ordinary_and_literal_values() {
    for schema in [
        json!({"type": "string", "description": "ordinary", "enum": ["a", "b"]}),
        json!({"type": "STRING", "nullable": true, "format": "date"}),
        json!({"type": "STRING", "nullable": false}),
        json!({"type": "object", "default": {"type": ["string", "null"], "nullable": false},
            "enum": [{"type": ["object", "null"]}], "example": {"type": ["array", "null"]}}),
    ] {
        assert_schema_egress(schema.clone(), schema);
    }
}

#[test]
fn malformed_or_unsupported_schema_unions_are_not_narrowed() {
    for schema in [
        json!({"type": []}),
        json!({"type": ["null"]}),
        json!({"type": ["string", "number", "null"]}),
        json!({"type": ["string", "number"]}),
        json!({"type": ["null", "null"]}),
        json!({"type": ["unknown", "null"]}),
        json!({"type": [true, "null"]}),
        json!({"type": ["string", "null"], "nullable": false}),
        json!({"type": ["string", "null"], "nullable": "true"}),
        json!({"type": "object", "properties": [{"type": ["string", "null"]}]}),
    ] {
        assert_schema_egress(schema.clone(), schema);
    }
}

#[test]
fn native_nullable_schemas_survive_the_existing_reader_writer_path() {
    let schema = json!({"type": "OBJECT", "properties": {
        "value": {"type": "STRING", "nullable": true, "description": "preserved"}
    }});
    let request = GeminiReader
        .read_request(&json!({
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
            "generationConfig": {"responseMimeType": "application/json", "responseSchema": schema},
            "tools": [{"functionDeclarations": [{"name": "lookup", "parameters": schema}]}]
        }))
        .expect("native request");
    assert_eq!(
        request.tools[0]
            .input_schema
            .pointer("/properties/value/type"),
        Some(&json!(["string", "null"]))
    );
    let wire = GeminiWriter.write_request(crate::codec::test_host::HOST, &request);
    for path in [
        "/generationConfig/responseSchema",
        "/tools/0/functionDeclarations/0/parameters",
    ] {
        let result = wire.pointer(path).unwrap();
        assert_eq!(
            result.pointer("/properties/value"),
            schema.pointer("/properties/value")
        );
    }
}
