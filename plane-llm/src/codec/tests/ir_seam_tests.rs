// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! IR mapping — the cross-protocol SEAM: what `chat_prepare_for_ingress` and the
//! shared OpenAI annotation mapping keep. Probe-style: a real upstream body goes through the egress
//! dialect's reader, the production seam step, and the ingress dialect's writer, exactly as the
//! forward path runs them.

use serde_json::{json, Value};

#[test]
fn cohere_folded_documents_keep_their_distinct_drop_origins() {
    use crate::codec::{drops, proto_codec::protocol_for, test_host::HOST};
    let cohere = protocol_for("cohere").unwrap();
    let anthropic = protocol_for("anthropic").unwrap();
    // Cohere's declared document input accepts objects; an opaque document has no foreign handle.
    let document = json!({"id":"same-document"});
    let body = json!({
        "model":"m",
        "documents":[document.clone()],
        "messages":[
            {"role":"system","content":"instruction"},
            {"role":"user","content":[
                {"type":"document","document":document.clone()},
                {"type":"text","text":"answer this"}
            ]}
        ]
    });
    let request = cohere.reader().read_request(&body).unwrap();
    let cloned = request.clone();
    assert_eq!(request, cloned);
    for request in [&request, &cloned] {
        let (out, paths) = drops::scope(
            drops::Seam {
                direction: drops::Direction::Request,
                ingress: "cohere",
                egress: "anthropic",
            },
            || anthropic.writer().write_request(HOST, request),
        );
        assert_eq!(paths, ["documents[]", "messages[].content[].type=document"]);
        assert_eq!(
            out["messages"][0]["content"],
            json!([{"type":"text","text":"answer this"}])
        );
        assert_eq!(out["system"], json!([{"type":"text","text":"instruction"}]));
    }
    let native = cohere.writer().write_request(HOST, &request);
    assert_eq!(native["documents"], json!([document.clone(), document]));
}

#[test]
fn cohere_tool_plan_and_thinking_parts_keep_their_distinct_drop_origins() {
    use crate::codec::{drops, proto_codec::protocol_for, test_host::HOST};
    let cohere = protocol_for("cohere").unwrap();
    let openai = protocol_for("openai").unwrap();
    let anthropic = protocol_for("anthropic").unwrap();
    let content = json!([
        {"type":"thinking","thinking":"same thought"},
        {"type":"thinking","thinking":"same thought"},
        {"type":"text","text":"answer"}
    ]);
    let response = cohere.reader().read_response(HOST, &json!({
        "id":"r","message":{"role":"assistant","tool_plan":"same thought","content":content.clone()},
        "finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":3,"output_tokens":4}}
    })).unwrap();
    let (out, paths) = drops::scope(
        drops::Seam {
            direction: drops::Direction::Response,
            ingress: "openai",
            egress: "cohere",
        },
        || openai.writer().write_response(HOST, &response.clone()),
    );
    assert_eq!(
        paths,
        ["message.tool_plan", "message.content[].type=thinking"]
    );
    assert_eq!(out["choices"][0]["message"]["content"], "answer");
    assert_eq!(out["usage"]["prompt_tokens"], 3);
    assert_eq!(out["usage"]["completion_tokens"], 4);
    let request = cohere.reader().read_request(&json!({
        "model":"m","messages":[{"role":"assistant","tool_plan":"same thought","content":content}]
    })).unwrap();
    let (out, paths) = drops::scope(
        drops::Seam {
            direction: drops::Direction::Request,
            ingress: "cohere",
            egress: "anthropic",
        },
        || anthropic.writer().write_request(HOST, &request),
    );
    assert_eq!(
        paths,
        ["messages[].tool_plan", "messages[].content[].type=thinking"]
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([{"type":"text","text":"answer"}])
    );
}

#[test]
fn cohere_tool_result_children_keep_the_native_message_content_path() {
    use crate::codec::{drops, proto_codec::protocol_for, test_host::HOST};
    let cohere = protocol_for("cohere").unwrap();
    let anthropic = protocol_for("anthropic").unwrap();
    let request = cohere.reader().read_request(&json!({
        "model":"m","messages":[
            {"role":"assistant","tool_calls":[{"id":"t","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"t","content":[
                {"type":"text","text":"before"},
                {"type":"document","document":{"id":"opaque"}},
                {"type":"text","text":"after"}
            ]}
        ]
    })).unwrap();
    let (out, paths) = drops::scope(
        drops::Seam {
            direction: drops::Direction::Request,
            ingress: "cohere",
            egress: "anthropic",
        },
        || anthropic.writer().write_request(HOST, &request),
    );
    assert_eq!(paths, ["messages[].content[].type=document"]);
    assert_eq!(
        out["messages"][1]["content"][0]["content"],
        json!([{"type":"text","text":"before"},{"type":"text","text":"after"}])
    );
}

#[test]
fn translated_upstream_failure_is_terminal_even_with_open_blocks_or_deferred_success() {
    use crate::codec::proto_stream::StreamTranslate;
    let host = crate::codec::test_host::HOST;
    let opening = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"usage\":{\"input_tokens\":17,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"before\"}}\n\n"
    );
    let stop = concat!(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let error = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"failed\"}}\n\n";
    for ingress in ["bedrock", "cohere", "openai"] {
        for deferred_success in [false, true] {
            let mut stream = StreamTranslate::new(ingress, "anthropic").unwrap();
            assert!(!stream.feed(host, opening.as_bytes()).is_empty());
            if deferred_success {
                stream.feed(host, stop.as_bytes());
            }
            let mut failed = stream.feed(host, error.as_bytes());
            assert!(!failed.is_empty(), "{ingress}");
            if ingress == "bedrock" {
                let frames = crate::codec::eventstream::drain_frames(&mut failed);
                assert!(failed.is_empty());
                assert_eq!(frames.len(), if deferred_success { 1 } else { 2 });
                if !deferred_success {
                    assert_eq!(frames[0].0, "contentBlockStop");
                }
                assert_eq!(frames.last().unwrap().0, "serviceUnavailableException");
            }
            let tail = stream.finish(host);
            if ingress == "openai" {
                assert_eq!(tail, b"data: [DONE]\n\n");
            } else {
                assert!(tail.is_empty(), "{ingress}: {tail:?}");
            }
            // Late upstream frames still report work, but cannot reopen the failed client stream.
            assert!(stream.feed(host, opening.as_bytes()).is_empty());
            assert!(stream.feed(host, stop.as_bytes()).is_empty());
            let usage = stream.usage().unwrap();
            assert_eq!((usage.input_tokens, usage.output_tokens), (17, 7));
            assert!(stream.terminal_error().is_some());
            assert!(!stream.local_encoding_failure());
        }
    }
}

#[test]
fn streamed_writer_media_drops_reach_the_existing_audit_paths() {
    use crate::codec::proto_stream::StreamTranslate;
    let host = crate::codec::test_host::HOST;
    let bodies = [
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"before"}]}}]}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"AQID"}}]}}]}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"text":"after"}]}}]}),
        json!({"candidates":[{"content":{"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"BAUG"}}]}}]}),
    ];
    for ingress in ["bedrock", "cohere"] {
        let mut stream = StreamTranslate::new(ingress, "gemini").unwrap();
        let mut bytes = Vec::new();
        for body in &bodies {
            bytes.extend(stream.feed(host, format!("data: {body}\n\n").as_bytes()));
        }
        bytes.extend(stream.finish(host));
        assert_eq!(
            stream
                .dropped()
                .iter()
                .filter(|path| path.as_str() == "candidates[].content.parts[]")
                .count(),
            1,
            "{ingress}: {:?}",
            stream.dropped()
        );
        let frames: Vec<Value> = if ingress == "bedrock" {
            crate::codec::eventstream::drain_frames(&mut bytes)
                .into_iter()
                .map(|(_, payload)| serde_json::from_slice(&payload).unwrap())
                .collect()
        } else {
            std::str::from_utf8(&bytes)
                .unwrap()
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|payload| serde_json::from_str(payload).unwrap())
                .collect()
        };
        let after = frames
            .iter()
            .find(|value| {
                value.pointer("/delta/text") == Some(&json!("after"))
                    || value.pointer("/delta/message/content/text") == Some(&json!("after"))
            })
            .unwrap();
        assert_eq!(
            after
                .get("contentBlockIndex")
                .or_else(|| after.get("index")),
            Some(&json!(1))
        );
    }
    let mut native = StreamTranslate::new_same_proto("gemini").unwrap();
    let wire = format!("data: {}\n\n", bodies[1]);
    assert_eq!(native.feed(host, wire.as_bytes()), wire.as_bytes());
    assert!(native.dropped().is_empty());
}

#[test]
fn cohere_answer_citations_keep_answer_offsets_and_never_become_document_offsets() {
    let citation = json!({"start":2,"end":5,"text":"abc","sources":[{"type":"document","document":{"url":"https://source.example","title":"source"}}]});
    let body = json!({"id":"cohere-1","message":{"role":"assistant","content":[{"type":"text","text":"x abc y"}],"citations":[citation.clone()]},"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":3,"output_tokens":4}}});
    let written = xresp("cohere", "bedrock", &body);
    let projected = &written["output"]["message"]["content"][0]["citationsContent"]["citations"][0];
    assert_eq!(
        projected["location"]["web"]["url"], "https://source.example",
        "{written}"
    );
    assert!(
        projected["location"].get("documentChar").is_none(),
        "{written}"
    );
    assert_eq!(written["usage"]["inputTokens"], 3);
    assert_eq!(written["usage"]["outputTokens"], 4);
    let responses = xresp("cohere", "responses", &body);
    let annotations = url_citations(&responses);
    assert_eq!(annotations.len(), 1, "{responses}");
    assert_eq!(annotations[0]["start_index"], 2);
    assert_eq!(annotations[0]["end_index"], 5);
    let native = xresp("cohere", "cohere", &body);
    assert_eq!(native["message"]["citations"][0], citation);
    let document = json!({"id":"msg_1","role":"assistant","content":[{"type":"text","text":"abc","citations":[{"type":"char_location","cited_text":"abc","document_index":3,"start_char_index":10,"end_char_index":13,"document_title":null,"file_id":null}]}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4}});
    let written = xresp("anthropic", "bedrock", &document);
    assert_eq!(
        written["output"]["message"]["content"][0]["citationsContent"]["citations"][0]["location"]
            ["documentChar"],
        json!({"documentIndex":3,"start":10,"end":13})
    );
}

#[test]
fn bedrock_exception_ends_stream_index_lifetime() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};
    let protocol = crate::codec::proto_codec::protocol_for("bedrock").unwrap();
    let host = crate::codec::test_host::HOST;
    let emit = |event: &IrStreamEvent| protocol.writer().write_response_events(host, event);
    emit(&stream_message_start("first"));
    emit(&IrStreamEvent::BlockStart {
        index: 5,
        block: IrBlockMeta::Text,
        refusal: false,
    });
    let error = busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("upstream failed".into()),
        retry_after: None,
    };
    assert!(protocol
        .writer()
        .write_response_exception(host, &error)
        .is_some());
    emit(&stream_message_start("second"));
    assert!(emit(&IrStreamEvent::BlockStop { index: 5 }).is_empty());
    emit(&IrStreamEvent::BlockStart {
        index: 8,
        block: IrBlockMeta::Text,
        refusal: false,
    });
    let frames = emit(&IrStreamEvent::BlockDelta {
        index: 8,
        delta: IrDelta::TextDelta("new stream".into()),
    });
    assert_eq!(mapped_stream_index(&frames[0]), Some(0));
}

/// Buffered cross-protocol response: egress reader -> `chat_prepare_for_ingress` -> ingress writer.
fn xresp(egress: &str, ingress: &'static str, body: &Value) -> Value {
    let egress_p = crate::codec::proto_codec::protocol_for(egress).expect("egress dialect");
    let ingress_p = crate::codec::proto_codec::protocol_for(ingress).expect("ingress dialect");
    let mut ir = egress_p
        .reader()
        .read_response(crate::codec::test_host::HOST, body)
        .expect("egress reader accepts the body");
    crate::codec::chat_handle::chat_prepare_for_ingress(&mut ir, ingress, 1_752_000_000);
    ingress_p
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir)
}

/// Every `url_citation` annotation anywhere in a written body, whichever OpenAI shape it has.
fn url_citations(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut stack = vec![v];
    while let Some(n) = stack.pop() {
        match n {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("url_citation") {
                    // Chat nests the fields under `url_citation`; Responses flattens them.
                    out.push(m.get("url_citation").cloned().unwrap_or_else(|| n.clone()));
                }
                stack.extend(m.values());
            }
            Value::Array(a) => stack.extend(a.iter()),
            _ => {}
        }
    }
    out
}

/// SHR-01: an OpenAI Chat backend's `url_citation` (with its character span) reaches a Responses
/// client. Before, the shared reader dropped the span and the Responses writer, which requires one,
/// dropped the whole citation — `annotations: []`.
#[test]
fn shr01_chat_citation_with_its_span_reaches_a_responses_client() {
    let body = json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt",
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant", "content": "hello world",
            "annotations": [{"type": "url_citation", "url_citation": {
                "url": "https://c.example", "title": "t", "start_index": 6, "end_index": 11}}]}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    });
    let out = xresp("openai", "responses", &body);
    let cites = url_citations(&out);
    assert_eq!(
        cites.len(),
        1,
        "citation lost on openai -> responses: {out}"
    );
    assert_eq!(cites[0]["url"], "https://c.example");
    assert_eq!(cites[0]["start_index"], 6);
    assert_eq!(cites[0]["end_index"], 11);
}

/// SHR-01, the reverse hop: a Responses backend's citation keeps its span for a Chat client.
#[test]
fn shr01_responses_citation_keeps_its_span_for_a_chat_client() {
    let body = json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "completed",
        "model": "gpt",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "hello world", "annotations": [
                {"type": "url_citation", "url": "https://c.example", "title": "t",
                 "start_index": 6, "end_index": 11}]}]}],
        "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
    });
    let out = xresp("responses", "openai", &body);
    let cites = url_citations(&out);
    assert_eq!(
        cites.len(),
        1,
        "citation lost on responses -> openai: {out}"
    );
    assert_eq!(cites[0]["start_index"], 6, "span dropped: {out}");
    assert_eq!(cites[0]["end_index"], 11, "span dropped: {out}");
}

/// The span rule's other half: offsets a citation carries into the CITED DOCUMENT (an Anthropic
/// `char_location`) are not a span of the answer text and must never be written as one.
#[test]
fn shr01_document_offsets_are_never_written_as_an_answer_span() {
    let c = crate::codec::ir::IrCitation {
        kind: Some("char_location".to_string()),
        url: Some("https://doc.example".to_string()),
        start_index: Some(0),
        end_index: Some(5),
        ..Default::default()
    };
    let chat = crate::codec::url_citation_wire::chat_url_annotations(
        "hello world",
        0,
        std::slice::from_ref(&c),
    );
    assert_eq!(chat.len(), 1);
    assert!(
        chat[0]["url_citation"].get("start_index").is_none(),
        "a document offset was written as an answer span: {chat:?}"
    );
    // A text-span kind keeps its carried span.
    let web = crate::codec::ir::IrCitation {
        kind: Some("web_search_result_location".to_string()),
        ..c
    };
    let chat = crate::codec::url_citation_wire::chat_url_annotations("hello world", 0, &[web]);
    assert_eq!(chat[0]["url_citation"]["start_index"], 0);
    assert_eq!(chat[0]["url_citation"]["end_index"], 5);
}

/// SEAM (`chat_prepare_for_ingress`): the matched stop string survives the buffered seam. A
/// Bedrock Converse body carries it (`additionalModelResponseFields.stop_sequence`) and the
/// Anthropic writer emits it; the seam used to null it, so the buffered answer said `null` where
/// the stream path (`MessageDelta.stop_sequence`, never stripped) kept it. Driven from the IR the
/// egress reader hands the seam, so it pins the seam step itself whichever reader filled the field.
#[test]
fn seam_matched_stop_sequence_survives_the_buffered_seam() {
    let mut ir = crate::codec::ir::IrResponse {
        content: vec![crate::codec::ir::IrBlock::Text {
            text: "one two".to_string(),
            cache_control: None,
            citations: vec![],
            refusal: false,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::StopSequence),
        stop_sequence: Some("###".to_string()),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 3,
            output_tokens: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    crate::codec::chat_handle::chat_prepare_for_ingress(&mut ir, "anthropic", 1_752_000_000);
    let out = crate::codec::proto_codec::protocol_for("anthropic")
        .expect("anthropic")
        .writer()
        .write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(out["stop_reason"], "stop_sequence", "{out}");
    assert_eq!(
        out["stop_sequence"], "###",
        "matched stop string lost: {out}"
    );
}

/// The seam still normalizes what no target can carry: the backend's id format never reaches a
/// different dialect's client, and `system_fingerprint` (OpenAI Chat only) is cleared.
#[test]
fn seam_still_mints_a_native_id_and_clears_the_chat_only_fingerprint() {
    let body = json!({
        "id": "chatcmpl-abc", "object": "chat.completion", "created": 1, "model": "gpt",
        "system_fingerprint": "fp_1",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "hi"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let out = xresp("openai", "anthropic", &body);
    let id = out["id"].as_str().expect("anthropic id");
    assert!(id.starts_with("msg_"), "foreign id leaked: {out}");
    assert!(out.get("system_fingerprint").is_none(), "{out}");
}

/// SHR-03: one OpenAI Files id namespace, two dialect tags. The shared helper reads the id off a
/// reference either OpenAI reader produced, and nothing else.
#[test]
fn shr03_openai_file_id_is_read_from_either_openai_dialects_reference() {
    use crate::codec::ir::IrImageSource;
    for vendor in crate::codec::url_citation_wire::FILES_API_VENDOR_TAGS {
        let src = IrImageSource::Vendor {
            vendor,
            value: json!({"file_id": "file-abc"}),
        };
        assert_eq!(
            crate::codec::url_citation_wire::files_api_id(&src),
            Some("file-abc"),
            "{vendor}"
        );
    }
    let s3 = IrImageSource::Vendor {
        vendor: "bedrock",
        value: json!({"file_id": "file-abc"}),
    };
    assert_eq!(crate::codec::url_citation_wire::files_api_id(&s3), None);
    let empty = IrImageSource::Vendor {
        vendor: "openai",
        value: json!({"file_id": ""}),
    };
    assert_eq!(crate::codec::url_citation_wire::files_api_id(&empty), None);
}

/// The tags ARE what the two OpenAI readers stamp: a Chat `file.file_id` part and a Responses
/// `input_file.file_id` part both read into a reference the helper resolves — if either reader
/// renames its tag, this fails rather than the id silently stopping to cross.
#[test]
fn shr03_both_openai_readers_produce_a_reference_the_helper_resolves() {
    fn first_media_source(ir: &crate::codec::ir::IrRequest) -> crate::codec::ir::IrImageSource {
        ir.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                crate::codec::ir::IrBlock::Media { source, .. } => Some(source.clone()),
                _ => None,
            })
            .expect("a Media block")
    }
    let chat = json!({"model": "m", "messages": [{"role": "user", "content": [
        {"type": "file", "file": {"file_id": "file-abc"}}]}]});
    let responses = json!({"model": "m", "input": [{"role": "user", "content": [
        {"type": "input_file", "file_id": "file-abc"}]}]});
    for (dialect, body) in [("openai", chat), ("responses", responses)] {
        let ir = crate::codec::proto_codec::protocol_for(dialect)
            .expect("dialect")
            .reader()
            .read_request(&body)
            .expect("read_request");
        let src = first_media_source(&ir);
        assert_eq!(
            crate::codec::url_citation_wire::files_api_id(&src),
            Some("file-abc"),
            "{dialect}: {src:?}"
        );
    }
}

/// Through the production write (`write_egress_request` with the lane model), a cross-protocol
/// reasoning ask writes no Converse `thinking` on ANY Bedrock lane, Claude or Nova, as in 1.5.5
/// (v1.5.5 `crates/busbar/src/proto/bedrock/writer.rs:139-146`), so it bills no thinking tokens 1.5.5
/// did not bill. RED while a Claude lane received `thinking {enabled}`.
#[test]
fn bedrock_thinking_is_written_for_no_lane_model() {
    use busbar_contract::ir::handle::IrHandle;
    let body = json!({
        "model": "claude-x", "max_tokens": 8192,
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = crate::codec::proto_codec::protocol_for("anthropic")
        .expect("anthropic")
        .reader()
        .read_request(&body)
        .expect("read");
    let write = |model: &str| {
        let mut h = crate::codec::chat_handle::ChatReqHandle(ir.clone(), Default::default());
        match h.write_egress_request(crate::codec::test_host::HOST, "bedrock", model) {
            busbar_contract::codec::EgressWire::Json(v) => v,
            _ => panic!("chat writes JSON"),
        }
    };
    for model in [
        "anthropic.claude-sonnet-4-5-20250929-v1:0",
        "us.anthropic.claude-opus-4-1-20250805-v1:0",
        "arn:aws:bedrock:us-east-1:123:inference-profile/global.anthropic.claude-sonnet-4-5",
        "amazon.nova-pro-v1:0",
    ] {
        let out = write(model);
        assert!(
            out.pointer("/additionalModelRequestFields/thinking")
                .is_none(),
            "{model}: {out}"
        );
    }
}

#[test]
fn overlapping_quotes_do_not_fabricate_an_annotation_span() {
    use crate::codec::ir::IrCitation;
    use crate::codec::url_citation_wire::{chat_url_annotations, url_annotations};
    let citation = |quote: &str| IrCitation {
        kind: Some("web_search_result_location".to_owned()),
        cited_text: Some(quote.to_owned()),
        url: Some("https://source.example".to_owned()),
        ..Default::default()
    };
    for (text, quote) in [("aaaa", "aaa"), ("ééé", "éé"), ("🦀ééé", "éé")] {
        let citations = [citation(quote)];
        assert!(
            url_annotations(text, 7, &citations).is_empty(),
            "ambiguous quote in {text:?}"
        );
        assert_eq!(
            chat_url_annotations(text, 7, &citations),
            [json!({
                "type": "url_citation",
                "url_citation": {"url": "https://source.example", "title": "https://source.example"}
            })],
            "Chat keeps the source without inventing offsets for {text:?}"
        );
    }
    for (text, quote, start, end) in [("baaa!", "aaa", 8, 11), ("🦀éaé", "éa", 8, 10)] {
        let citations = [citation(quote)];
        let responses = url_annotations(text, 7, &citations);
        let chat = chat_url_annotations(text, 7, &citations);
        assert_eq!(responses[0]["start_index"], start);
        assert_eq!(responses[0]["end_index"], end);
        assert_eq!(chat[0]["url_citation"]["start_index"], start);
        assert_eq!(chat[0]["url_citation"]["end_index"], end);
    }
    let carried = IrCitation {
        start_index: Some(1),
        end_index: Some(4),
        ..citation("aaa")
    };
    let responses = url_annotations("aaaa", 7, std::slice::from_ref(&carried));
    let chat = chat_url_annotations("aaaa", 7, &[carried]);
    assert_eq!(responses[0]["start_index"], 8);
    assert_eq!(responses[0]["end_index"], 11);
    assert_eq!(chat[0]["url_citation"]["start_index"], 8);
    assert_eq!(chat[0]["url_citation"]["end_index"], 11);
}

#[test]
fn tail_usage_recovery_ignores_later_nonobject_members_named_usage() {
    use crate::codec::usage_tail::isolate_tail_usage_object;
    for suffix in [
        r#","metadata":{"usage":"echo"}}"#,
        r#","metadata":{"usage":null}}"#,
        r#","metadata":{"usage":17}}"#,
    ] {
        let tail = format!(
            r#"{{"content":"received content","usage":{{"prompt_tokens":7,"completion_tokens":11}}{suffix}"#
        );
        assert_eq!(
            isolate_tail_usage_object(tail.as_bytes(), b"\"usage\""),
            Some(json!({"prompt_tokens":7,"completion_tokens":11}))
        );
    }
    assert_eq!(
        isolate_tail_usage_object(
            br#"{"content":"received","usage": {"prompt_tokens":7},"other":true}"#,
            b"\"usage\""
        ),
        Some(json!({"prompt_tokens":7}))
    );
    assert!(isolate_tail_usage_object(br#"{"metadata":{"usage":"echo"}}"#, b"\"usage\"").is_none());
}

#[test]
fn tail_usage_recovery_accepts_only_json_whitespace() {
    use crate::codec::usage_tail::isolate_tail_usage_object;
    for byte in [0x85, 0xa0, 0x0b, 0x0c] {
        for position in [0, 1] {
            let mut tail = b"{\"usage\"".to_vec();
            if position == 0 {
                tail.push(byte);
            }
            tail.push(b':');
            if position == 1 {
                tail.push(byte);
            }
            tail.extend_from_slice(br#"{"prompt_tokens":7}"#);
            assert!(
                isolate_tail_usage_object(&tail, b"\"usage\"").is_none(),
                "invalid JSON whitespace {byte:#x} at {position}"
            );
        }
    }
    assert_eq!(
        isolate_tail_usage_object(
            b"{\"usage\" \t\r\n: \t\r\n{\"prompt_tokens\":7}",
            b"\"usage\""
        ),
        Some(json!({"prompt_tokens":7}))
    );
}

#[test]
fn rerank_readers_refuse_malformed_entries_without_compacting_indices() {
    use crate::codec::proto_codec::entry_of;
    for protocol in ["cohere", "bedrock"] {
        let codec = entry_of(protocol).unwrap().leaf.rerank.as_ref().unwrap();
        for documents in [
            json!(["first", 17, "last"]),
            json!(["first",{"other":"bad"},"last"]),
        ] {
            let body = serde_json::to_vec(&json!({"model":"m","query":"q","documents":documents}))
                .unwrap();
            assert!(
                (codec.read_request)(&body, "application/json").is_err(),
                "{protocol} must refuse a document that would shift later result indices"
            );
        }
        let valid = br#"{"model":"m","query":"q","documents":["first",{"text":"last"}]}"#;
        assert_eq!(
            (codec.read_request)(valid, "application/json")
                .unwrap()
                .documents,
            ["first", "last"]
        );
        for result in [
            json!({"index":1}),
            json!({"index":1,"relevance_score":"bad"}),
            json!({"relevance_score":0.9}),
            json!({"index":-1,"relevance_score":0.9}),
        ] {
            let body =
                serde_json::to_vec(&json!({"results":[{"index":0,"relevance_score":1.0},result]}))
                    .unwrap();
            assert!(
                (codec.read_response)(&body).is_err(),
                "{protocol} must refuse malformed ranking"
            );
        }
        let valid =
            br#"{"results":[{"index":1,"relevance_score":0.9,"document":{"text":"last"}}]}"#;
        let response = (codec.read_response)(valid).unwrap();
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].index, 1);
        assert_eq!(response.results[0].document.as_deref(), Some("last"));
    }
}

#[test]
fn cohere_embeddings_keep_input_positions_or_refuse_the_request() {
    use crate::codec::ir::embeddings::EmbInput;
    let codec = crate::codec::proto_codec::entry_of("cohere")
        .unwrap()
        .leaf
        .embeddings
        .as_ref()
        .unwrap();
    for texts in [json!(["first", 17, "last"]), json!(["first", null, "last"])] {
        let body = serde_json::to_vec(&json!({"model":"m","texts":texts})).unwrap();
        assert!((codec.read_request)(&body, "application/json").is_err());
    }
    let good = (codec.read_request)(
        br#"{"model":"m","texts":["first","last"]}"#,
        "application/json",
    )
    .unwrap();
    assert_eq!(
        good.input,
        EmbInput::Text(vec!["first".into(), "last".into()])
    );
}

#[test]
fn cohere_embedding_vectors_never_shrink_or_wrap_malformed_cells() {
    use crate::codec::ir::embeddings::{EncFmt, VectorData};
    let codec = crate::codec::proto_codec::entry_of("cohere")
        .unwrap()
        .leaf
        .embeddings
        .as_ref()
        .unwrap();
    for embeddings in [
        json!({"float":[[1.0,"bad",2.0]]}),
        json!({"float":[[1.0,1e100]]}),
        json!({"int8":[[1,null,2]]}),
        json!({"int8":[[1,2147483648_i64]]}),
        json!({"uint8":[[1,-2147483649_i64]]}),
    ] {
        let body = serde_json::to_vec(&json!({"embeddings":embeddings})).unwrap();
        assert!(
            (codec.read_response)(&body).is_err(),
            "malformed embedding must not look valid"
        );
    }
    let good=(codec.read_response)(br#"{"embeddings":{"float":[[1.5,-2.0]],"int8":[[127,-128]]},"meta":{"billed_units":{"input_tokens":7}}}"#).unwrap();
    assert_eq!(
        good.embeddings[0].vectors[&EncFmt::Float],
        VectorData::Float(vec![1.5, -2.0])
    );
    assert_eq!(
        good.embeddings[0].vectors[&EncFmt::Int8],
        VectorData::Int(vec![127, -128])
    );
    assert_eq!(good.usage.unwrap().input, 7);
}

#[test]
fn bedrock_web_citations_do_not_reinterpret_answer_offsets_as_document_offsets() {
    let body = json!({"id":"chatcmpl-web","object":"chat.completion","created":1,"model":"m",
        "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"quoted text",
            "annotations":[{"type":"url_citation","url_citation":{"url":"https://source.example",
                "title":"source","start_index":0,"end_index":6}}]}}],
        "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}});
    let written = xresp("openai", "bedrock", &body);
    let citation = &written["output"]["message"]["content"][0]["citationsContent"]["citations"][0];
    assert_eq!(
        citation["location"]["web"]["url"], "https://source.example",
        "{written}"
    );
    assert!(
        citation["location"].get("documentChar").is_none(),
        "{written}"
    );
    assert_eq!(written["usage"]["inputTokens"], 3);
    assert_eq!(written["usage"]["outputTokens"], 4);
}

#[test]
fn cohere_requests_refuse_unpairable_or_malformed_tool_calls() {
    let protocol = crate::codec::proto_codec::protocol_for("cohere").unwrap();
    for call in [
        json!({"id":"call"}),
        json!({"id":"call","function":{}}),
        json!({"id":"call","function":{"name":"f","arguments":"bad json"}}),
        json!({"id":"call","function":{"name":"f","arguments":{}}}),
    ] {
        let body = json!({"model":"m","messages":[{"role":"assistant","tool_calls":[call]}]});
        assert!(
            protocol.reader().read_request(&body).is_err(),
            "malformed call must be refused: {body}"
        );
    }
    for body in [
        json!({"model":"m","messages":[{"role":"tool","content":"result"}]}),
        json!({"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{}]}),
        json!({"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"function":{}}]}),
    ] {
        assert!(
            protocol.reader().read_request(&body).is_err(),
            "unpairable tool must be refused"
        );
    }
    let body = json!({"model":"m","messages":[
        {"role":"assistant","tool_calls":[{"id":"call","function":{"name":"f","arguments":"{\"x\":7}"}}]},
        {"role":"tool","tool_call_id":"call","content":"result"}]});
    let request = protocol.reader().read_request(&body).unwrap();
    assert!(
        matches!(&request.messages[0].content[0],crate::codec::ir::IrBlock::ToolUse {id,name,input,..}
                    if id=="call" && name=="f" && input==&json!({"x":7}))
    );
    assert!(
        matches!(&request.messages[1].content[0],crate::codec::ir::IrBlock::ToolResult {tool_use_id,..}
                    if tool_use_id=="call")
    );
}

#[test]
fn unreadable_usage_attribution_warns_without_changing_billed_totals() {
    use crate::codec::usage_count::{read_usage, refused_any, CountRead, CountSlot};
    use busbar_test_kit::warn_capture::WarnCapture;
    let table = [
        (CountSlot::Input, CountRead::Zero(&["input"])),
        (CountSlot::Output, CountRead::Zero(&["output"])),
        (
            CountSlot::Reasoning,
            CountRead::Lenient(&["reasoning_tokens"]),
        ),
        (
            CountSlot::OutputAudio,
            CountRead::ListFirst {
                list: &["modalities"],
                key: "modality",
                value: "audio",
                count: "audio_tokens",
            },
        ),
    ];
    let body = json!({"input":10,"output":20,"reasoning_tokens":"bad",
                    "modalities":[{"modality":"audio","audio_tokens":"bad"}]});
    let capture = WarnCapture::default();
    let usage = tracing::subscriber::with_default(capture.clone(), || {
        read_usage("test", Some(&body), &table)
    })
    .unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 20));
    assert_eq!(usage.detail.reasoning_tokens, None);
    assert_eq!(usage.detail.output_audio_tokens, None);
    assert!(!refused_any(Some(&body), &table));
    assert!(capture.contains("reasoning_tokens"));
    assert!(capture.contains("audio_tokens"));
    let body = json!({"input":10,"output":20,"reasoning_tokens":7,
                    "modalities":[{"modality":"audio","audio_tokens":0}]});
    let usage = read_usage("test", Some(&body), &table).unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 20));
    assert_eq!(usage.detail.reasoning_tokens, Some(7));
    assert_eq!(usage.detail.output_audio_tokens, Some(0));
}

#[test]
fn cohere_requests_refuse_malformed_output_and_tool_controls() {
    let protocol = crate::codec::proto_codec::protocol_for("cohere").unwrap();
    for control in [
        json!({"max_tokens":4294967296_u64}),
        json!({"max_tokens":0}),
        json!({"max_tokens":"10"}),
    ] {
        let mut body = json!({"model":"m","messages":[{"role":"user","content":"q"}]});
        body.as_object_mut()
            .unwrap()
            .extend(control.as_object().unwrap().clone());
        assert!(
            protocol.reader().read_request(&body).is_err(),
            "control must not disappear: {body}"
        );
    }
    for value in [json!("required"), json!({})] {
        use busbar_test_kit::warn_capture::WarnCapture;
        let capture = WarnCapture::default();
        let body =
            json!({"model":"m","messages":[{"role":"user","content":"q"}],"tool_choice":value});
        let request = tracing::subscriber::with_default(capture.clone(), || {
            protocol.reader().read_request(&body)
        })
        .unwrap();
        assert_eq!(request.tool_choice, None);
        assert!(capture.contains("unrecognized Cohere tool_choice"));
    }
    let optional = json!({"model":"m","messages":[{"role":"assistant","tool_calls":[
        {"id":"call","function":{"name":"f"}}]}],"tools":null,"max_tokens":null});
    let request = protocol.reader().read_request(&optional).unwrap();
    assert_eq!(request.max_tokens, None);
    assert!(
        matches!(&request.messages[0].content[0],crate::codec::ir::IrBlock::ToolUse {input,..}
                    if input==&json!({}))
    );
    let body = json!({"model":"m","messages":[{"role":"user","content":"q"}],
                    "max_tokens":10,"tool_choice":"NONE"});
    let request = protocol.reader().read_request(&body).unwrap();
    assert_eq!(request.max_tokens, Some(10));
    assert_eq!(
        request.tool_choice,
        Some(crate::codec::ir::IrToolChoice::None)
    );
}

#[test]
fn cohere_targeted_tool_degradation_is_visible() {
    use busbar_test_kit::warn_capture::WarnCapture;
    let protocol = crate::codec::proto_codec::protocol_for("cohere").unwrap();
    let mut request = crate::codec::ir::IrRequest::default();
    request.tools.push(crate::codec::ir::IrTool {
        name: "chosen".into(),
        input_schema: json!({"type":"object"}),
        description: None,
        cache_control: None,
        hosted: None,
        strict: None,
    });
    request.tool_choice = Some(crate::codec::ir::IrToolChoice::Tool {
        name: "chosen".into(),
    });
    let capture = WarnCapture::default();
    let written = tracing::subscriber::with_default(capture.clone(), || {
        protocol
            .writer()
            .write_request(crate::codec::test_host::HOST, &request)
    });
    assert_eq!(written["tool_choice"], "REQUIRED");
    assert!(capture.contains("targeted tool constraint"));
}

#[test]
fn bedrock_video_names_never_become_unsupported_wire_members() {
    use crate::codec::ir::{IrBlock, IrImageSource, IrMediaKind, IrMessage, IrRequest, IrRole};
    use busbar_test_kit::warn_capture::WarnCapture;
    let protocol = crate::codec::proto_codec::protocol_for("bedrock").unwrap();
    for nested in [false, true] {
        let media = IrBlock::Media {
            kind: IrMediaKind::Video,
            source: IrImageSource::Base64 {
                media_type: "video/webm".into(),
                data: "GkXfow==".into(),
            },
            name: Some("clip.webm".into()),
            cache_control: None,
            citations: None,
            context: None,
        };
        let content = if nested {
            vec![IrBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![media],
                is_error: false,
                cache_control: None,
            }]
        } else {
            vec![media]
        };
        let request = IrRequest {
            messages: vec![IrMessage {
                role: IrRole::User,
                content,
            }],
            ..Default::default()
        };
        let capture = WarnCapture::default();
        let written = tracing::subscriber::with_default(capture.clone(), || {
            protocol
                .writer()
                .write_request(crate::codec::test_host::HOST, &request)
        });
        let video = if nested {
            &written["messages"][0]["content"][0]["toolResult"]["content"][0]["video"]
        } else {
            &written["messages"][0]["content"][0]["video"]
        };
        assert_eq!(video["format"], "webm", "{written}");
        assert_eq!(video["source"]["bytes"], "GkXfow==", "{written}");
        assert!(video.get("name").is_none(), "{written}");
        assert!(capture.contains("VideoBlock"));
    }
}

#[test]
fn leaf_usage_taps_preserve_exact_counts_when_the_payload_is_malformed() {
    use busbar_contract::codec::RequestHandler;
    use busbar_contract::operation::OpVerb;
    let cohere = crate::codec::cohere::handler::CohereRequestHandler;
    let bedrock = crate::codec::bedrock::handler::BedrockRequestHandler;
    let rerank_handlers: [(&str, &dyn RequestHandler); 2] =
        [("cohere", &cohere), ("bedrock", &bedrock)];
    for (protocol, handler) in rerank_handlers {
        let op = handler.operation_handler(OpVerb::RERANK).unwrap();
        let body=br#"{"results":[{"index":0}],"meta":{"billed_units":{"search_units":18446744073709551615}}}"#;
        assert!(op
            .read_response(crate::codec::test_host::HOST, body)
            .is_err());
        let usage = op
            .extract_usage(crate::codec::test_host::HOST, protocol, body)
            .unwrap()
            .unwrap();
        assert_eq!(usage.open_units.get("search_units"), Some(&u64::MAX));
        assert_eq!((usage.input, usage.output), (0, 0));
        for units in [json!(null), json!("bad")] {
            let body = serde_json::to_vec(
                &json!({"results":[],"meta":{"billed_units":{"search_units":units}}}),
            )
            .unwrap();
            assert!(op
                .extract_usage(crate::codec::test_host::HOST, protocol, &body)
                .unwrap()
                .is_none());
        }
    }
    let op = cohere.operation_handler(OpVerb::EMBEDDINGS).unwrap();
    let body=br#"{"embeddings":{"float":[[1,"bad"]]},"meta":{"billed_units":{"input_tokens":18446744073709551615}}}"#;
    assert!(op
        .read_response(crate::codec::test_host::HOST, body)
        .is_err());
    let usage = op
        .extract_usage(crate::codec::test_host::HOST, "cohere", body)
        .unwrap()
        .unwrap();
    assert_eq!(usage.input, u64::MAX);
    assert_eq!(usage.output, 0);
}

#[test]
fn distinct_dropped_block_kinds_have_distinct_audit_paths() {
    use crate::codec::drops::{self, Direction, Dropped, Seam};
    use busbar_test_kit::warn_capture::WarnCapture;
    let request = crate::codec::proto_codec::protocol_for("anthropic")
        .unwrap()
        .reader()
        .read_request(&json!({"model":"m", "max_tokens":32, "messages":[
            {"role":"user", "content":[
                {"type":"image", "source":{"type":"url", "url":"https://example.invalid/a.png"}},
                {"type":"document", "source":{"type":"text", "media_type":"text/plain", "data":"document"}}
            ]}
        ]}))
        .unwrap();
    let capture = WarnCapture::default();
    let (_, paths) = tracing::subscriber::with_default(capture.clone(), || {
        drops::scope(
            Seam {
                direction: Direction::Request,
                ingress: "anthropic",
                egress: "cohere",
            },
            || {
                for (member, index) in [(drops::IMAGE, 0), (drops::DOCUMENT, 1), (drops::IMAGE, 0)]
                {
                    drops::with_block_origin(request.block_origins.messages[0].get(index), || {
                        drops::note(Dropped::member(
                            member,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            "dropped",
                        ));
                    });
                }
            },
        )
    });
    assert_eq!(
        paths,
        [
            "messages[].content[].type=image",
            "messages[].content[].type=document"
        ]
    );
    assert_eq!(capture.count("path=messages[].content[].type=image"), 1);
    assert_eq!(capture.count("path=messages[].content[].type=document"), 1);
    let (_, paths) = drops::scope(
        Seam {
            direction: Direction::Request,
            ingress: "anthropic",
            egress: "cohere",
        },
        || {
            drops::note(Dropped::member(
                drops::IMAGE,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                "dropped",
            ))
        },
    );
    assert_eq!(
        paths,
        ["ir.block.kind=image"],
        "missing provenance stays explicit"
    );
}

#[test]
fn drop_diagnostics_allow_subscriber_reentry_without_a_borrow_panic() {
    use crate::codec::drops::{self, Direction, Dropped, Seam};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Reentrant(Arc<AtomicUsize>);
    impl tracing::Subscriber for Reentrant {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            assert!(drops::is_open());
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                drops::note(Dropped::new(
                    "subscriber-path",
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    "subscriber",
                ));
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let events = Arc::new(AtomicUsize::new(0));
    let (_, paths) = tracing::subscriber::with_default(Reentrant(events.clone()), || {
        drops::scope(
            Seam {
                direction: Direction::Request,
                ingress: "openai",
                egress: "cohere",
            },
            || {
                drops::note(Dropped::new(
                    "original-path",
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    "original",
                ));
            },
        )
    });
    assert_eq!(paths, ["original-path", "subscriber-path"]);
    assert!(events.load(Ordering::Relaxed) > 0);
    assert!(!drops::is_open());
}

#[test]
fn malformed_logprob_bytes_never_become_shortened_valid_tokens() {
    use crate::codec::logprob_wire::{read_token_logprobs, write_token_logprobs};
    use busbar_test_kit::warn_capture::WarnCapture;
    let capture = WarnCapture::default();
    let body = json!({"content":[
        {"token":"bad","logprob":-1.0,"bytes":[226,null,153]},
        {"token":"bad","logprob":-1.0,"bytes":[1,256,2]},
        {"token":"valid","logprob":-0.5,"bytes":[226,128],"top_logprobs":[
            {"token":"bad top","logprob":-2.0,"bytes":[1,-1,2]},
            {"token":"valid top","logprob":-1.0,"bytes":[153]}]},
        {"token":"missing-logprob","bytes":[1]}]});
    let entries =
        tracing::subscriber::with_default(capture.clone(), || read_token_logprobs(Some(&body)));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].bytes.as_deref(), Some(&[226, 128][..]));
    assert_eq!(entries[0].top.len(), 1);
    assert_eq!(entries[0].top[0].bytes.as_deref(), Some(&[153][..]));
    let written = write_token_logprobs(&entries);
    assert_eq!(written["content"][0]["bytes"], json!([226, 128]));
    assert!(capture.contains("dropping malformed logprob entry"));
    let absent = json!({"content":[{"token":"hi","logprob":-0.5}]});
    let written = write_token_logprobs(&read_token_logprobs(Some(&absent)));
    assert_eq!(
        written["content"][0]["bytes"],
        json!([104, 105]),
        "existing unknown-bytes synthesis policy stays unchanged"
    );
}

#[test]
fn logprob_null_bytes_survive_independently_for_chosen_and_top_tokens() {
    use crate::codec::logprob_wire::{read_token_logprobs, write_token_logprobs};
    let controls = [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!([226, 128])),
    ];
    for chosen_bytes in &controls {
        for top_bytes in &controls {
            let mut body = json!({"content":[{
                "token":"<|x|>", "logprob":-0.5,
                "top_logprobs":[{"token":"é", "logprob":-1.0}]
            }]});
            if let Some(bytes) = chosen_bytes {
                body["content"][0]["bytes"] = bytes.clone();
            }
            if let Some(bytes) = top_bytes {
                body["content"][0]["top_logprobs"][0]["bytes"] = bytes.clone();
            }
            let written = write_token_logprobs(&read_token_logprobs(Some(&body)));
            let expected_chosen = chosen_bytes.clone().unwrap_or_else(|| json!(b"<|x|>"));
            let expected_top = top_bytes.clone().unwrap_or_else(|| json!([195, 169]));
            assert_eq!(written["content"][0]["bytes"], expected_chosen);
            assert_eq!(
                written["content"][0]["top_logprobs"][0]["bytes"],
                expected_top
            );
            assert_eq!(written["content"][0]["token"], "<|x|>");
            assert_eq!(written["content"][0]["logprob"], -0.5);
            assert_eq!(written["content"][0]["top_logprobs"][0]["token"], "é");
            assert_eq!(written["content"][0]["top_logprobs"][0]["logprob"], -1.0);
        }
    }
}

#[test]
fn chat_reader_and_writer_preserve_explicit_null_logprob_bytes() {
    let host = crate::codec::test_host::HOST;
    let body = json!({
        "id":"chatcmpl-bytes", "object":"chat.completion", "created":1, "model":"m",
        "choices":[{"index":0, "finish_reason":"stop",
            "message":{"role":"assistant", "content":"<|x|>"},
            "logprobs":{"content":[{"token":"<|x|>", "logprob":-0.5, "bytes":null,
                "top_logprobs":[{"token":"é", "logprob":-1.0, "bytes":null}]}],
                "refusal":null}}],
        "usage":{"prompt_tokens":17, "completion_tokens":9, "total_tokens":26}
    });
    let protocol = crate::codec::proto_codec::protocol_for("openai").unwrap();
    let response = protocol.reader().read_response(host, &body).unwrap();
    let written = protocol.writer().write_response(host, &response);
    let chosen = &written["choices"][0]["logprobs"]["content"][0];
    assert_eq!(chosen.get("bytes"), Some(&serde_json::Value::Null));
    assert_eq!(
        chosen["top_logprobs"][0].get("bytes"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(written["choices"][0]["message"]["content"], "<|x|>");
    assert_eq!(written["usage"]["prompt_tokens"], 17);
    assert_eq!(written["usage"]["completion_tokens"], 9);
}

#[test]
fn bedrock_unrepresentable_document_names_use_the_existing_neutral_label() {
    use crate::codec::ir::{IrBlock, IrImageSource, IrMediaKind, IrMessage, IrRequest, IrRole};
    use busbar_test_kit::warn_capture::WarnCapture;
    let protocol = crate::codec::proto_codec::protocol_for("bedrock").unwrap();
    for name in ["Q3_report.v2.pdf", "two  spaces", ""] {
        let request = IrRequest {
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrBlock::Media {
                    kind: IrMediaKind::Document,
                    source: IrImageSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "YQ==".into(),
                    },
                    name: Some(name.into()),
                    cache_control: None,
                    citations: None,
                    context: None,
                }],
            }],
            ..Default::default()
        };
        let capture = WarnCapture::default();
        let written = tracing::subscriber::with_default(capture.clone(), || {
            protocol
                .writer()
                .write_request(crate::codec::test_host::HOST, &request)
        });
        let doc = &written["messages"][0]["content"][0]["document"];
        assert_eq!(doc["name"], "attachment", "{written}");
        assert_eq!(doc["source"]["bytes"], "YQ==", "{written}");
        assert!(capture.contains("neutral attachment label"));
    }
    let body = json!({"messages":[{"role":"user","content":[{"document":{
        "name":"Q3 Report-(final)[2]","format":"pdf","source":{"bytes":"YQ=="}}}]}]});
    let request = protocol.reader().read_request(&body).unwrap();
    let written = protocol
        .writer()
        .write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(
        written["messages"][0]["content"][0]["document"]["name"],
        "Q3 Report-(final)[2]"
    );

    let native = json!({"model":"m", "messages":[{"role":"user", "content":[{
        "type":"file", "file":{"file_data":"data:application/pdf;base64,JVBERi0=", "filename":"spec.pdf"}
    }]}]});
    let openai = crate::codec::proto_codec::protocol_for("openai").unwrap();
    let request = openai.reader().read_request(&native).unwrap();
    let capture = WarnCapture::default();
    let (written, dropped) = tracing::subscriber::with_default(capture.clone(), || {
        crate::codec::drops::scope(
            crate::codec::drops::Seam {
                direction: crate::codec::drops::Direction::Request,
                ingress: "openai",
                egress: "bedrock",
            },
            || {
                protocol
                    .writer()
                    .write_request(crate::codec::test_host::HOST, &request)
            },
        )
    });
    let document = &written["messages"][0]["content"][0]["document"];
    assert_eq!(document["name"], "attachment", "{written}");
    assert_eq!(document["source"]["bytes"], "JVBERi0=", "{written}");
    assert_eq!(dropped, ["messages[].content[].type=file"]);
    assert!(capture.contains("neutral attachment label"));
}

#[test]
fn cohere_writer_refuses_tools_and_citations_that_lose_ir_meaning() {
    use crate::codec::chat_handle::ChatRespHandle;
    use crate::codec::ir::{IrBlock, IrCitation, IrResponse, IrUsage};
    use busbar_contract::{codec::TranslatedResponse, ir::handle::IrHandle};
    let call = |id: &str, input: Value| IrBlock::ToolUse {
        id: id.into(),
        name: "f".into(),
        input,
        cache_control: None,
        thought_signature: None,
    };
    let citation = IrCitation {
        document_index: Some(3),
        ..Default::default()
    };
    for block in [
        call("", json!({})),
        call("call", json!("not JSON")),
        IrBlock::Text {
            text: "answer".into(),
            citations: vec![citation],
            cache_control: None,
            refusal: false,
        },
    ] {
        let handle = ChatRespHandle(
            IrResponse {
                id: Some("native-id".into()),
                content: vec![block],
                usage: IrUsage {
                    input_tokens: 3,
                    output_tokens: 4,
                    ..Default::default()
                },
                ..Default::default()
            },
            4096,
        );
        assert!(matches!(
            handle.write_ingress_response(crate::codec::test_host::HOST, "cohere", true),
            TranslatedResponse::Untranslatable
        ));
        assert_eq!(handle.token_usage().unwrap().input, 3);
        assert_eq!(handle.token_usage().unwrap().output, 4);
    }
    let handle = ChatRespHandle(
        IrResponse {
            id: Some("native-id".into()),
            content: vec![call("call", json!({"x":7}))],
            ..Default::default()
        },
        4096,
    );
    let TranslatedResponse::Json(written) =
        handle.write_ingress_response(crate::codec::test_host::HOST, "cohere", true)
    else {
        panic!("valid tool remains deliverable")
    };
    assert_eq!(written["message"]["tool_calls"][0]["id"], "call");
}

#[test]
fn cohere_tool_validation_preserves_replayed_plans_and_grounding() {
    use crate::codec::ir::IrBlock;
    let protocol = crate::codec::proto_codec::protocol_for("cohere").unwrap();
    for content in [json!("answer"), json!(null)] {
        let body = json!({"model":"m","messages":[{"role":"assistant","content":content,
            "tool_plan":"plan first","citations":[{"start":0,"end":6,"text":"answer",
            "sources":[{"type":"document","document":{"title":"source","url":"https://source.example"}}]}],
            "tool_calls":[{"id":"call","function":{"name":"f","arguments":"{}"}}]}]});
        let request = protocol.reader().read_request(&body).unwrap();
        let blocks = &request.messages[0].content;
        assert!(matches!(&blocks[0],IrBlock::Thinking {text,..} if text=="plan first"));
        let citations: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                IrBlock::Text {
                    text, citations, ..
                } => Some((text, citations)),
                _ => None,
            })
            .collect();
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].1.len(), 1);
        assert_eq!(
            citations[0].1[0].url.as_deref(),
            Some("https://source.example")
        );
        assert!(matches!(blocks.last().unwrap(),IrBlock::ToolUse {id,..} if id=="call"));
    }
}

fn mapped_stream_index(frame: &(String, Value)) -> Option<u64> {
    frame
        .1
        .get("contentBlockIndex")
        .or_else(|| frame.1.get("index"))
        .and_then(Value::as_u64)
}

fn stream_message_start(id: &str) -> crate::codec::ir::IrStreamEvent {
    crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        id: Some(id.into()),
        created: None,
        model: Some("m".into()),
        usage: None,
    }
}

#[test]
fn dropped_stream_media_does_not_reserve_a_wire_block_index() {
    use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrImageSource, IrStreamEvent, IrUsage};
    use busbar_test_kit::warn_capture::WarnCapture;
    for name in ["bedrock", "cohere"] {
        for leading_media in [false, true] {
            let protocol = crate::codec::proto_codec::protocol_for(name).unwrap();
            let emit = |event: &IrStreamEvent| {
                protocol
                    .writer()
                    .write_response_events(crate::codec::test_host::HOST, event)
            };
            emit(&stream_message_start("id"));
            let capture = WarnCapture::default();
            let frames = tracing::subscriber::with_default(capture.clone(), || {
                let mut frames = Vec::new();
                if !leading_media {
                    frames.extend(emit(&IrStreamEvent::BlockStart {
                        index: 0,
                        block: IrBlockMeta::Text,
                        refusal: false,
                    }));
                    frames.extend(emit(&IrStreamEvent::BlockDelta {
                        index: 0,
                        delta: IrDelta::TextDelta("before".into()),
                    }));
                    frames.extend(emit(&IrStreamEvent::BlockStop { index: 0 }));
                }
                let media_index = if leading_media { 0 } else { 1 };
                assert!(emit(&IrStreamEvent::BlockStart {
                    index: media_index,
                    block: IrBlockMeta::Image,
                    refusal: false
                })
                .is_empty());
                assert!(emit(&IrStreamEvent::BlockDelta {
                    index: media_index,
                    delta: IrDelta::MediaDelta(Box::new(IrBlock::Image {
                        source: IrImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "AQID".into()
                        },
                        cache_control: None,
                        detail: None
                    }))
                })
                .is_empty());
                assert!(emit(&IrStreamEvent::BlockStop { index: media_index }).is_empty());
                let text_index = media_index + 1;
                frames.extend(emit(&IrStreamEvent::BlockStart {
                    index: text_index,
                    block: IrBlockMeta::Text,
                    refusal: false,
                }));
                frames.extend(emit(&IrStreamEvent::BlockDelta {
                    index: text_index,
                    delta: IrDelta::TextDelta("after".into()),
                }));
                frames.extend(emit(&IrStreamEvent::BlockStop { index: text_index }));
                frames.extend(emit(&IrStreamEvent::MessageDelta {
                    stop_reason: None,
                    stop_sequence: None,
                    stop_detail: None,
                    usage: IrUsage {
                        input_tokens: u64::MAX,
                        output_tokens: 7,
                        ..Default::default()
                    },
                }));
                frames
            });
            let after = frames
                .iter()
                .find(|(_, value)| {
                    value.pointer("/delta/text") == Some(&json!("after"))
                        || value.pointer("/delta/message/content/text") == Some(&json!("after"))
                })
                .unwrap();
            assert_eq!(
                mapped_stream_index(after),
                Some(if leading_media { 0 } else { 1 }),
                "{name} {frames:?}"
            );
            assert!(capture.contains("dropping generated media"));
            let usage = frames
                .iter()
                .find_map(|(_, value)| {
                    value
                        .pointer("/usage/inputTokens")
                        .or_else(|| value.pointer("/delta/usage/tokens/input_tokens"))
                })
                .unwrap();
            assert_eq!(usage, &json!(u64::MAX));
        }
    }
}

#[test]
fn stream_block_mapping_survives_reverse_closes_late_citations_and_clone() {
    use crate::codec::ir::{IrBlockMeta, IrCitation, IrDelta, IrStreamEvent};
    for name in ["bedrock", "cohere"] {
        let protocol = crate::codec::proto_codec::protocol_for(name).unwrap();
        let emit = |event: &IrStreamEvent| {
            protocol
                .writer()
                .write_response_events(crate::codec::test_host::HOST, event)
        };
        emit(&stream_message_start("id"));
        for (index, block) in [
            (0, IrBlockMeta::Text),
            (1, IrBlockMeta::Image),
            (2, IrBlockMeta::Text),
            (3, IrBlockMeta::Image),
            (
                4,
                IrBlockMeta::ToolUse {
                    id: "call".into(),
                    name: "f".into(),
                },
            ),
        ] {
            emit(&IrStreamEvent::BlockStart {
                index,
                block,
                refusal: false,
            });
        }
        let args = emit(&IrStreamEvent::BlockDelta {
            index: 4,
            delta: IrDelta::InputJsonDelta("{}".into()),
        });
        assert_eq!(mapped_stream_index(&args[0]), Some(2));
        let cloned = protocol.clone();
        for (index, wire) in [(4, 2), (2, 1), (0, 0)] {
            let frames = emit(&IrStreamEvent::BlockStop { index });
            assert_eq!(frames.len(), 1);
            assert_eq!(mapped_stream_index(&frames[0]), Some(wire), "{name}");
        }
        assert!(emit(&IrStreamEvent::BlockStop { index: 1 }).is_empty());
        assert!(emit(&IrStreamEvent::BlockStop { index: 3 }).is_empty());
        let citation = IrStreamEvent::BlockDelta {
            index: 2,
            delta: IrDelta::CitationsDelta(vec![IrCitation {
                title: Some("source".into()),
                url: Some("https://source.example".into()),
                ..Default::default()
            }]),
        };
        let frames = emit(&citation);
        assert!(!frames.is_empty());
        assert!(
            frames
                .iter()
                .all(|frame| mapped_stream_index(frame) == Some(1)),
            "{name} {frames:?}"
        );
        let frames = cloned.writer().write_response_events(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStop { index: 4 },
        );
        assert_eq!(
            mapped_stream_index(&frames[0]),
            Some(2),
            "clone preserves its in-flight block mapping"
        );
    }
}

#[test]
fn stream_index_reset_preserves_duplicate_start_identity_and_delta_only_calls() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStreamEvent};
    for name in ["bedrock", "cohere"] {
        let protocol = crate::codec::proto_codec::protocol_for(name).unwrap();
        let emit = |event: &IrStreamEvent| {
            protocol
                .writer()
                .write_response_events(crate::codec::test_host::HOST, event)
        };
        emit(&stream_message_start("first"));
        emit(&IrStreamEvent::BlockStart {
            index: 5,
            block: IrBlockMeta::Text,
            refusal: false,
        });
        let duplicate = emit(&stream_message_start("duplicate"));
        if name == "cohere" {
            assert_eq!(duplicate[0].1["id"], "first");
        }
        let frames = emit(&IrStreamEvent::BlockDelta {
            index: 5,
            delta: IrDelta::TextDelta("still-first".into()),
        });
        assert_eq!(mapped_stream_index(&frames[0]), Some(0));
        emit(&IrStreamEvent::BlockStart {
            index: 6,
            block: IrBlockMeta::Text,
            refusal: false,
        });
        let frames = emit(&IrStreamEvent::BlockDelta {
            index: 6,
            delta: IrDelta::TextDelta("next".into()),
        });
        assert_eq!(mapped_stream_index(&frames[0]), Some(1));
        emit(&IrStreamEvent::MessageStop);
        // A late block from the retired stream must not cancel the next message's reset.
        emit(&IrStreamEvent::BlockStart {
            index: 90,
            block: IrBlockMeta::Text,
            refusal: false,
        });
        let restarted = emit(&stream_message_start("second"));
        if name == "cohere" {
            assert_eq!(restarted[0].1["id"], "second");
        }
        emit(&IrStreamEvent::BlockStart {
            index: 8,
            block: IrBlockMeta::Text,
            refusal: false,
        });
        let frames = emit(&IrStreamEvent::BlockDelta {
            index: 8,
            delta: IrDelta::TextDelta("reset".into()),
        });
        assert_eq!(mapped_stream_index(&frames[0]), Some(0));
        let direct = crate::codec::proto_codec::protocol_for(name).unwrap();
        let frames = direct.writer().write_response_events(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 17,
                delta: IrDelta::TextDelta("direct".into()),
            },
        );
        assert_eq!(mapped_stream_index(&frames[0]), Some(17));
    }
}

#[test]
fn gemini_interleaved_media_keeps_dense_bedrock_and_cohere_text_indices() {
    use crate::codec::ir::StreamDecodeState;
    for name in ["bedrock", "cohere"] {
        let gemini = crate::codec::proto_codec::protocol_for("gemini").unwrap();
        let target = crate::codec::proto_codec::protocol_for(name).unwrap();
        let mut state = StreamDecodeState::default();
        let mut frames = Vec::new();
        for parts in [
            json!([{"text":"before"}]),
            json!([{"inlineData":{"mimeType":"image/png","data":"AQID"}}]),
            json!([{"text":"after"}]),
        ] {
            let body = json!({"candidates":[{"content":{"role":"model","parts":parts}}]});
            for event in gemini.reader().read_response_events(
                crate::codec::test_host::HOST,
                "",
                &body,
                &mut state,
            ) {
                frames.extend(
                    target
                        .writer()
                        .write_response_events(crate::codec::test_host::HOST, &event),
                );
            }
        }
        let indices: Vec<_> = frames
            .iter()
            .filter(|(_, value)| {
                value.pointer("/delta/text").is_some()
                    || value
                        .pointer("/delta/message/content/text")
                        .is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty()))
            })
            .filter_map(mapped_stream_index)
            .collect();
        assert_eq!(indices, [0, 1], "{name} {frames:?}");
    }
}

#[test]
fn malformed_leaf_recovery_keeps_full_billing_variants() {
    use busbar_contract::{billing::Billing, codec::RequestHandler, operation::OpVerb};
    let cohere = crate::codec::cohere::handler::CohereRequestHandler;
    let bedrock = crate::codec::bedrock::handler::BedrockRequestHandler;
    let handlers: [(&str, &dyn RequestHandler); 2] = [("cohere", &cohere), ("bedrock", &bedrock)];
    for (_, handler) in handlers {
        let op = handler.operation_handler(OpVerb::RERANK).unwrap();
        for count in [0, 7, u64::MAX] {
            let malformed = serde_json::to_vec(&json!({"results":[{"index":0}],
                "meta":{"billed_units":{"search_units":count}}}))
            .unwrap();
            assert!(op
                .read_response(crate::codec::test_host::HOST, &malformed)
                .is_err());
            let expected = Some(Billing::Counted {
                class: "search_units".into(),
                count,
            });
            assert_eq!(
                op.recover_response_billing(crate::codec::test_host::HOST, &malformed),
                expected
            );
            let valid = serde_json::to_vec(&json!({"results":[{"index":0,"relevance_score":0.9}],
                "meta":{"billed_units":{"search_units":count}}}))
            .unwrap();
            assert_eq!(
                op.read_response(crate::codec::test_host::HOST, &valid)
                    .unwrap()
                    .billing(),
                expected
            );
        }
        for count in [json!(null), json!("bad")] {
            let body = serde_json::to_vec(&json!({"results":[{"index":0}],
                "meta":{"billed_units":{"search_units":count}}}))
            .unwrap();
            assert_eq!(
                op.recover_response_billing(crate::codec::test_host::HOST, &body),
                Some(Billing::Flat)
            );
        }
        assert_eq!(
            op.recover_response_billing(
                crate::codec::test_host::HOST,
                br#"{"results":[{"index":0}]}"#
            ),
            Some(Billing::Flat)
        );
        assert_eq!(
            op.recover_response_billing(crate::codec::test_host::HOST, b"not JSON"),
            None
        );
    }
    let op = cohere.operation_handler(OpVerb::EMBEDDINGS).unwrap();
    let body=br#"{"embeddings":{"float":[[1,"bad"]]},"meta":{"billed_units":{"input_tokens":18446744073709551615}}}"#;
    let Some(Billing::Tokens(usage)) =
        op.recover_response_billing(crate::codec::test_host::HOST, body)
    else {
        panic!("embedding recovery must stay token-metered")
    };
    assert_eq!(usage.input, u64::MAX);
}

#[test]
fn cohere_stream_writer_refuses_unrepresentable_tool_and_citation_events() {
    use crate::codec::ir::{IrBlockMeta, IrCitation, IrDelta, IrStreamEvent};
    let protocol = crate::codec::proto_codec::protocol_for("cohere").unwrap();
    for event in [
        IrStreamEvent::BlockStart {
            index: 0,
            block: IrBlockMeta::ToolUse {
                id: "".into(),
                name: "f".into(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: IrDelta::CitationsDelta(vec![IrCitation {
                document_index: Some(3),
                ..Default::default()
            }]),
        },
    ] {
        assert!(protocol
            .writer()
            .try_write_response_events(crate::codec::test_host::HOST, &event)
            .is_err());
    }
    let event = IrStreamEvent::BlockStart {
        index: 0,
        block: IrBlockMeta::ToolUse {
            id: "call".into(),
            name: "f".into(),
        },
        refusal: false,
    };
    let events = protocol
        .writer()
        .try_write_response_events(crate::codec::test_host::HOST, &event)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["type"], "tool-call-start");
}
