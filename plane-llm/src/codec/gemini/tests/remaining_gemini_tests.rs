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

#[test]
fn cohere_citations_shift_block_local_spans_in_response_and_history() {
    let response = GeminiReader
        .read_response(
            crate::codec::test_host::HOST,
            &json!({"candidates":[two_parts()]}),
        )
        .unwrap();
    let writer = protocol_for("cohere").unwrap();
    let wire = writer
        .writer()
        .write_response(crate::codec::test_host::HOST, &response);
    assert_eq!(
        wire.pointer("/message/citations/0/start"),
        Some(&json!(5)),
        "{wire}"
    );
    assert_eq!(
        wire.pointer("/message/citations/0/end"),
        Some(&json!(7)),
        "{wire}"
    );
    let mut request = schema_request(json!({"type":"object"}));
    request.messages = vec![crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::Assistant,
        content: response.content,
    }];
    let wire = writer
        .writer()
        .write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(
        wire.pointer("/messages/0/citations/0/start"),
        Some(&json!(5)),
        "{wire}"
    );
    assert_eq!(
        wire.pointer("/messages/0/citations/0/end"),
        Some(&json!(7)),
        "{wire}"
    );
}

#[test]
fn document_coordinates_are_replaced_only_by_an_unambiguous_answer_quote() {
    for (kind, start_key, end_key) in [
        ("char_location", "start_char_index", "end_char_index"),
        ("page_location", "start_page_number", "end_page_number"),
        (
            "content_block_location",
            "start_block_index",
            "end_block_index",
        ),
    ] {
        for (quote, answer, anchored) in [
            ("é中", "Bé中B", true),
            ("source-only quote", "Bé中B", false),
            ("é中", "Bé中é中B", false),
        ] {
            let native = json!({"id":"msg-native","type":"message","role":"assistant","model":"m",
                "content":[{"type":"text","text":"AAAA"},{"type":"text","text":answer,
                    "citations":[{"type":kind,"document_index":0,"document_title":"Doc",
                        (start_key):90,(end_key):92,"cited_text":quote}]}],
                "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}});
            let response = protocol_for("anthropic")
                .unwrap()
                .reader()
                .read_response(crate::codec::test_host::HOST, &native)
                .unwrap();
            let cohere = protocol_for("cohere")
                .unwrap()
                .writer()
                .write_response(crate::codec::test_host::HOST, &response);
            let citation = cohere.pointer("/message/citations/0").unwrap();
            let (gemini, dropped) = crate::codec::drops::scope(
                crate::codec::drops::Seam {
                    direction: crate::codec::drops::Direction::Response,
                    ingress: "gemini",
                    egress: "anthropic",
                },
                || GeminiWriter.write_response(crate::codec::test_host::HOST, &response),
            );
            let source = gemini
                .pointer("/candidates/0/citationMetadata/citationSources/0")
                .unwrap();
            if anchored {
                assert_eq!(citation["start"], json!(5), "{kind}: {cohere}");
                assert_eq!(citation["end"], json!(7), "{kind}: {cohere}");
                assert_eq!(source["startIndex"], json!(5), "{kind}: {gemini}");
                assert_eq!(source["endIndex"], json!(10), "{kind}: {gemini}");
            } else {
                assert!(
                    citation.get("start").is_none() && citation.get("end").is_none(),
                    "{cohere}"
                );
                assert!(
                    source.get("startIndex").is_none() && source.get("endIndex").is_none(),
                    "{gemini}"
                );
                assert!(
                    !dropped.is_empty(),
                    "unanchored source coordinates must be audited"
                );
            }
            // The production buffered-to-stream synthesizer drives the same block bases.
            for ingress in ["cohere", "gemini"] {
                let mut prepared = response.clone();
                crate::codec::chat_handle::chat_prepare_for_ingress(&mut prepared, ingress, 1);
                let stream = crate::codec::proto_stream::StreamTranslate::synthesize_from_response(
                    crate::codec::test_host::HOST,
                    ingress,
                    &prepared,
                    4096,
                )
                .unwrap();
                let stream = String::from_utf8(stream).unwrap();
                let sources: Vec<Value> = stream
                    .split("\n\n")
                    .filter_map(|frame| {
                        let (_, data) = crate::codec::dialect::parse_sse_frame(frame.as_bytes())?;
                        let frame: Value = serde_json::from_str(&data).ok()?;
                        if ingress == "cohere" {
                            (frame["type"] == "citation-start")
                                .then(|| frame["delta"]["message"]["citations"].clone())
                        } else {
                            frame
                                .pointer("/candidates/0/citationMetadata/citationSources/0")
                                .cloned()
                        }
                    })
                    .collect();
                assert_eq!(sources.len(), 1, "{ingress}/{kind}: {stream}");
                let (start_key, end_key, end) = if ingress == "cohere" {
                    ("start", "end", 7)
                } else {
                    ("startIndex", "endIndex", 10)
                };
                if anchored {
                    assert_eq!(sources[0][start_key], json!(5), "{stream}");
                    assert_eq!(sources[0][end_key], json!(end), "{stream}");
                } else {
                    assert!(
                        sources[0].get(start_key).is_none() && sources[0].get(end_key).is_none(),
                        "{stream}"
                    );
                }
            }
        }
    }
}

#[test]
fn streamed_cohere_citations_use_the_preceding_answers_character_count() {
    let chunks = [
        json!({"candidates":[{"content":{"parts":[{"text":"AAAA"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{}}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":"Bé中B"}]}}]}),
        json!({"candidates":[{"citationMetadata":{"citationSources":[
            {"uri":"https://second","startIndex":5,"endIndex":10}]},"finishReason":"STOP"}]}),
    ];
    let mut translator =
        crate::codec::proto_stream::StreamTranslate::new("cohere", "gemini").unwrap();
    let mut output = Vec::new();
    for chunk in chunks {
        output.extend(translator.feed(
            crate::codec::test_host::HOST,
            format!("data: {chunk}\n\n").as_bytes(),
        ));
    }
    output.extend(translator.finish(crate::codec::test_host::HOST));
    let output = String::from_utf8(output).unwrap();
    let citations: Vec<Value> = output
        .split("\n\n")
        .filter_map(|frame| {
            let (_, data) = crate::codec::dialect::parse_sse_frame(frame.as_bytes())?;
            let frame: Value = serde_json::from_str(&data).ok()?;
            (frame["type"] == "citation-start")
                .then(|| frame["delta"]["message"]["citations"].clone())
        })
        .collect();
    assert_eq!(citations.len(), 1, "{output}");
    assert_eq!(citations[0]["start"], json!(5), "{output}");
    assert_eq!(citations[0]["end"], json!(7), "{output}");
}

#[test]
fn schema_instance_literals_are_opaque_to_every_schema_walk() {
    let literal = json!({"const":"keep","examples":[1],"$ref":"#/$defs/Thing",
        "$defs":{"Fake":{"type":"number"}},"definitions":{"Literal":{"const":1}},
        "additionalProperties":{"const":true},"type":["string","null"]});
    let schema = json!({"type":"object", "$defs":{"Thing":{"type":"string","const":"remove"}},
        "default":literal,"enum":[literal],"example":literal,
        "properties":{"actual":{"$ref":"#/$defs/Thing","default":literal}},
        "anyOf":[{"type":"object","const":"remove","default":literal}]});
    let expected = json!({"type":"object", "default":literal,"enum":[literal],"example":literal,
        "properties":{"actual":{"type":"string","default":literal}},
        "anyOf":[{"type":"object","default":literal}]});
    assert_schema_egress(schema, expected);
    // A definition nested in instance data must not become a schema reference target.
    assert_schema_egress(
        json!({"default":{"$defs":{"Fake":{"type":"number"}}},
        "properties":{"value":{"$ref":"#/$defs/Fake"}}}),
        json!({"default":{"$defs":{"Fake":{"type":"number"}}},"properties":{"value":{}}}),
    );
}

#[test]
fn reflected_bad_key_phrases_remain_lane_healthy_client_errors() {
    for phrase in [
        "api key not valid",
        "invalid api-key",
        "api key has expired",
    ] {
        let body = serde_json::to_vec(&json!({"error":{"code":400,"status":"INVALID_ARGUMENT",
            "message":format!("Invalid JSON payload received. Unknown name \"{phrase}\" at 'contents[0]': Cannot find field.")}})).unwrap();
        let raw = GeminiReader.extract_error(StatusCode::BAD_REQUEST, &body);
        assert_eq!(raw.http_status, 400, "{raw:?}");
        let signal = crate::codec::test_host::classified(&raw);
        assert_ne!(signal.class, StatusClass::Auth, "{raw:?}");
        assert_ne!(
            signal.class.disposition(),
            busbar_contract::upstream::Disposition::HardDown
        );
    }
}

struct EntropyHost<'a> {
    writer: Option<&'a GeminiWriter>,
    mode: u8,
    draws: std::sync::atomic::AtomicUsize,
}

impl busbar_contract::codec::CodecHost for EntropyHost<'_> {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        let draw = self
            .draws
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Re-enter the real writer during the host call; a held stream lock would deadlock.
        if let Some(writer) = self.writer {
            writer.set_translate_cap(4096);
        }
        match self.mode {
            0 => {
                out.fill(7);
                false
            }
            1 => {
                out.fill(255);
                true
            }
            2 if draw == 0 => {
                out.fill(255);
                out[0] = 7;
                true
            }
            2 => false,
            _ => {
                out.fill(7);
                true
            }
        }
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}

#[test]
fn failed_or_partial_entropy_never_publishes_a_response_identity() {
    for mode in [0, 1, 2] {
        let writer = GeminiWriter;
        let host = EntropyHost {
            writer: Some(&writer),
            mode,
            draws: Default::default(),
        };
        let response = crate::codec::ir::IrResponse {
            created: Some(1),
            ..Default::default()
        };
        let wire = writer.write_response(&host, &response);
        assert!(wire.get("responseId").is_none(), "{wire}");
        host.draws.store(0, std::sync::atomic::Ordering::Relaxed);
        let (_, frame) = writer
            .write_response_event(
                &host,
                &IrStreamEvent::MessageStart {
                    role: crate::codec::ir::IrRole::Assistant,
                    usage: None,
                    id: None,
                    created: None,
                    model: Some("m".into()),
                },
            )
            .unwrap();
        assert!(frame.get("responseId").is_none(), "{frame}");
        assert_eq!(frame["modelVersion"], json!("m"));
    }
    let writer = GeminiWriter;
    let host = EntropyHost {
        writer: Some(&writer),
        mode: 3,
        draws: Default::default(),
    };
    let event = IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let first = writer.write_response_event(&host, &event).unwrap().1;
    let second = writer.write_response_event(&host, &event).unwrap().1;
    assert_eq!(first["responseId"], second["responseId"]);
    assert_eq!(host.draws.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn gemini_sources_without_anthropic_opaque_state_are_dropped_and_audited() {
    let source = json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},
        "citationMetadata":{"citationSources":[{"startIndex":0,"endIndex":6,"uri":"https://source","title":"Source"}]},
        "finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":9}});
    let mut translator =
        crate::codec::proto_stream::StreamTranslate::new("anthropic", "gemini").unwrap();
    let mut output = translator.feed(
        crate::codec::test_host::HOST,
        format!("data: {source}\n\n").as_bytes(),
    );
    output.extend(translator.finish(crate::codec::test_host::HOST));
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("answer"), "{output}");
    assert!(
        !output.contains("citations_delta") && !output.contains("encrypted_index"),
        "{output}"
    );
    assert!(
        !translator.dropped().is_empty(),
        "citation loss must be audited"
    );
    let usage = translator.usage().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 9));

    let response = GeminiReader
        .read_response(crate::codec::test_host::HOST, &source)
        .unwrap();
    let (wire, dropped) = crate::codec::drops::scope(
        crate::codec::drops::Seam {
            direction: crate::codec::drops::Direction::Response,
            ingress: "anthropic",
            egress: "gemini",
        },
        || {
            protocol_for("anthropic")
                .unwrap()
                .writer()
                .write_response(crate::codec::test_host::HOST, &response)
        },
    );
    assert!(!wire.to_string().contains("encrypted_index"), "{wire}");
    assert!(!dropped.is_empty(), "buffered loss must be audited");
}

#[test]
fn direct_gemini_writer_refuses_malformed_json_without_changing_valid_argument_policy() {
    for malformed in ["{\"recipient\":", "{bad}"] {
        let writer = GeminiWriter;
        writer.write_response_event(crate::codec::test_host::HOST, &tool_start(0, "send"));
        argument(&writer, 0, malformed);
        let (_, frame) = writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 0 },
            )
            .unwrap();
        assert_eq!(
            frame.pointer("/error/status"),
            Some(&json!("INTERNAL")),
            "{frame}"
        );
        assert!(frame.pointer("/candidates").is_none(), "{frame}");
        // Consuming a rejected tool cannot contaminate a later independent block.
        writer.write_response_event(crate::codec::test_host::HOST, &tool_start(1, "next"));
        argument(&writer, 1, "{\"x\":1}");
        let (_, frame) = writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 1 },
            )
            .unwrap();
        assert_eq!(
            frame.pointer("/candidates/0/content/parts/0/functionCall/args"),
            Some(&json!({"x":1}))
        );
    }
}

#[test]
fn actual_interleaved_responses_blocks_drop_unresolvable_coordinates_without_changing_other_blocks()
{
    let chunks = [
        (
            "response.created",
            json!({"type":"response.created","response":{"id":"resp-native","model":"m"}}),
        ),
        (
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","output_index":0,"delta":"foo"}),
        ),
        (
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","output_index":1,"delta":"bar"}),
        ),
        (
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","output_index":0,"delta":"baz"}),
        ),
        (
            "response.output_text.annotation.added",
            json!({"type":"response.output_text.annotation.added","output_index":0,
            "annotation":{"type":"url_citation","url":"https://interleaved","title":"Source","start_index":3,"end_index":6}}),
        ),
        (
            "response.output_text.annotation.added",
            json!({"type":"response.output_text.annotation.added","output_index":1,
            "annotation":{"type":"url_citation","url":"https://contiguous","title":"Source","start_index":0,"end_index":3}}),
        ),
    ];
    for ingress in ["cohere", "gemini"] {
        let mut translator =
            crate::codec::proto_stream::StreamTranslate::new(ingress, "responses").unwrap();
        let mut output = Vec::new();
        for (event, chunk) in &chunks {
            output.extend(translator.feed(
                crate::codec::test_host::HOST,
                format!("event: {event}\ndata: {chunk}\n\n").as_bytes(),
            ));
        }
        output.extend(translator.finish(crate::codec::test_host::HOST));
        let output = String::from_utf8(output).unwrap();
        let sources: Vec<Value> = output
            .split("\n\n")
            .filter_map(|frame| {
                let (_, data) = crate::codec::dialect::parse_sse_frame(frame.as_bytes())?;
                let frame: Value = serde_json::from_str(&data).ok()?;
                if ingress == "cohere" {
                    (frame["type"] == "citation-start")
                        .then(|| frame["delta"]["message"]["citations"].clone())
                } else {
                    frame
                        .pointer("/candidates/0/citationMetadata/citationSources/0")
                        .cloned()
                }
            })
            .collect();
        assert_eq!(sources.len(), 2, "{ingress}: {output}");
        let (start, end, url) = if ingress == "cohere" {
            ("start", "end", "/sources/0/document/url")
        } else {
            ("startIndex", "endIndex", "/uri")
        };
        assert_eq!(
            sources[0].pointer(url),
            Some(&json!("https://interleaved")),
            "{output}"
        );
        assert!(
            sources[0].get(start).is_none() && sources[0].get(end).is_none(),
            "{output}"
        );
        assert_eq!(
            sources[1].pointer(url),
            Some(&json!("https://contiguous")),
            "{output}"
        );
        assert_eq!(sources[1][start], json!(3), "{output}");
        assert_eq!(sources[1][end], json!(6), "{output}");
        assert!(
            !translator.dropped().is_empty(),
            "{ingress}: coordinate loss is audited"
        );
    }
}

#[test]
fn empty_gemini_writer_text_deltas_do_not_allocate_citation_anchors() {
    let writer = GeminiWriter;
    for index in 0..1024 {
        writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::TextDelta(String::new()),
            },
        );
    }
    assert!(writer.stream.lock().stream_text.block_starts.is_empty());
}
