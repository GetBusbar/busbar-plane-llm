use super::*;
use crate::codec::ir::StreamDecodeState;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

#[test]
fn buffered_provider_generation_errors_remain_failed_in_responses() {
    use crate::codec::ir::IrStopReason;
    let host = crate::codec::test_host::HOST;
    for (provider, body) in [
        (
            "gemini",
            serde_json::json!({
                "candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":11}
            }),
        ),
        (
            "gemini",
            serde_json::json!({
                "candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]},"finishReason":"UNEXPECTED_TOOL_CALL"}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":11}
            }),
        ),
        (
            "cohere",
            serde_json::json!({
                "message":{"role":"assistant","content":[{"type":"text","text":"partial"}]},"finish_reason":"ERROR",
                "usage":{"tokens":{"input_tokens":7,"output_tokens":11},"billed_units":{"input_tokens":7,"output_tokens":11}}
            }),
        ),
    ] {
        let protocol = crate::codec::proto_codec::protocol_for(provider).unwrap();
        let response = protocol
            .reader()
            .read_response(host, &body)
            .expect("supported provider response");
        assert_eq!(
            response.stop_reason,
            Some(IrStopReason::Error),
            "{provider}"
        );
        let writer = ResponsesWriter;
        let buffered = writer.write_response(host, &response);
        assert_eq!(buffered["status"], "failed", "{provider}: {buffered}");
        assert_eq!(buffered["error"]["code"], "server_error");
        assert_eq!(buffered["error"]["message"], "error");
        assert_eq!(buffered["output"][0]["content"][0]["text"], "partial");
        assert_eq!(buffered["usage"]["input_tokens"], 7);
        assert_eq!(buffered["usage"]["output_tokens"], 11);
        assert!(buffered["incomplete_details"].is_null());
        assert_eq!(
            ResponsesReader
                .read_response(host, &buffered)
                .unwrap_err()
                .class,
            StatusClass::ServerError
        );
        for (reason, status) in [
            (None, "completed"),
            (Some(IrStopReason::EndTurn), "completed"),
            (Some(IrStopReason::ToolUse), "completed"),
            (Some(IrStopReason::Refusal), "completed"),
            (Some(IrStopReason::Other), "completed"),
            (Some(IrStopReason::MaxTokens), "incomplete"),
            (Some(IrStopReason::Safety), "incomplete"),
        ] {
            let mut normal = response.clone();
            normal.stop_reason = reason;
            let writer = ResponsesWriter;
            let output = writer.write_response(host, &normal);
            assert_eq!(output["status"], status, "{reason:?}");
            assert!(output["error"].is_null(), "{reason:?}");
            assert_eq!(output["output"][0]["content"][0]["text"], "partial");
            assert_eq!(output["usage"]["output_tokens"], 11);
        }
    }
}

#[test]
fn buffered_responses_failed_usage_is_observable_without_masking_failure() {
    use crate::codec::cells::OperationHandler;
    let host = crate::codec::test_host::HOST;
    let handler = crate::codec::chat_handle::ChatOperation("responses");
    for (code, class) in [
        ("server_error", StatusClass::ServerError),
        ("rate_limit_exceeded", StatusClass::RateLimit),
        ("invalid_prompt", StatusClass::ClientError),
    ] {
        let failed = serde_json::json!({
            "status":"failed","output":[],"error":{"code":code,"message":"failed turn"},
            "usage":{"input_tokens":17,"output_tokens":11,"total_tokens":28,"input_tokens_details":{"cached_tokens":5}}
        });
        let error = ResponsesReader.read_response(host, &failed).unwrap_err();
        assert_eq!(error.class, class);
        assert_eq!(error.provider_signal.as_deref(), Some(code));
        let bytes = serde_json::to_vec(&failed).unwrap();
        let usage = handler
            .extract_usage(host, "responses", &bytes)
            .expect("reported usage tap")
            .expect("top-level usage is present");
        assert_eq!(
            (usage.input, usage.output, usage.cache_read),
            (12, 11, Some(5))
        );
    }
    for absent in [
        serde_json::json!({"status":"failed","output":[],"error":{"code":"server_error"}}),
        serde_json::json!({"status":"failed","output":[],"usage":null}),
        serde_json::json!({"status":"failed","output":[],"usage":"invalid"}),
        serde_json::json!({"status":"failed","output":[],"metadata":{"usage":{"input_tokens":999,"output_tokens":888}}}),
    ] {
        let bytes = serde_json::to_vec(&absent).unwrap();
        assert!(
            handler.extract_usage(host, "responses", &bytes).is_err(),
            "unreported/nested usage must not be invented: {absent}"
        );
    }
    let partially_readable =
        br#"{"status":"failed","output":[],"usage":{"input_tokens":"17","output_tokens":11}}"#;
    let usage = handler
        .extract_usage(host, "responses", partially_readable)
        .unwrap()
        .unwrap();
    assert_eq!((usage.input, usage.output), (0, 11), "existing per-count refusal preserves readable work without interpreting the string as a count");
}

#[test]
fn response_failed_codes_obey_pinned_response_schema_and_keep_native_codes() {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../testing/llm-conformance/wire/responses.wire.json"
    ))
    .unwrap();
    let legal = schema["response"]["error.code"]["enum"].as_array().unwrap();
    let host = crate::codec::test_host::HOST;
    for native in legal {
        let signal = native.as_str().unwrap();
        let writer = ResponsesWriter;
        let (_, frame) = writer
            .write_response_event(
                host,
                &IrStreamEvent::Error(IrError {
                    class: StatusClass::ServerError,
                    provider_signal: Some(signal.into()),
                    retry_after: None,
                }),
            )
            .unwrap();
        assert_eq!(&frame["response"]["error"]["code"], native);
        assert_eq!(frame["response"]["error"]["message"], signal);
    }
    for (class, expected) in [
        (StatusClass::RateLimit, "rate_limit_exceeded"),
        (StatusClass::ClientError, "invalid_prompt"),
        (StatusClass::ContextLength, "invalid_prompt"),
        (StatusClass::Auth, "server_error"),
        (StatusClass::Billing, "server_error"),
        (StatusClass::Overloaded, "server_error"),
        (StatusClass::ServerError, "server_error"),
        (StatusClass::Timeout, "server_error"),
        (StatusClass::Network, "server_error"),
    ] {
        for signal in [
            None,
            Some("boom"),
            Some("overloaded_error"),
            Some("connection reset by peer"),
        ] {
            let writer = ResponsesWriter;
            let error = IrError {
                class,
                provider_signal: signal.map(String::from),
                retry_after: None,
            };
            let original_signal = error.provider_signal.clone();
            let (_, frame) = writer
                .write_response_event(host, &IrStreamEvent::Error(error.clone()))
                .unwrap();
            let response = &frame["response"];
            assert_eq!(response["error"]["code"], expected, "{class:?} {signal:?}");
            assert!(legal.contains(&response["error"]["code"]), "{frame}");
            assert_eq!(response["error"]["message"], signal.unwrap_or("error"));
            assert_eq!(error.provider_signal, original_signal);
            assert_eq!(error.class, class);

            let represented = match expected {
                "rate_limit_exceeded" => StatusClass::RateLimit,
                "invalid_prompt" => StatusClass::ClientError,
                _ => StatusClass::ServerError,
            };
            assert_eq!(
                ResponsesReader
                    .read_response(host, response)
                    .unwrap_err()
                    .class,
                represented
            );
            let mut state = StreamDecodeState::default();
            let events =
                ResponsesReader.read_response_events(host, EVT_RESPONSE_FAILED, &frame, &mut state);
            assert!(events.iter().any(|event| matches!(event, IrStreamEvent::Error(error) if error.class == represented)), "{events:?}");
        }
    }
}

#[test]
fn response_failed_unrepresentable_code_is_diagnosed_without_changing_http_error_envelope() {
    let host = crate::codec::test_host::HOST;
    let seam = crate::codec::drops::Seam {
        direction: crate::codec::drops::Direction::Response,
        ingress: "responses",
        egress: "openai",
    };
    let (_, dropped) = crate::codec::drops::scope(seam, || {
        let writer = ResponsesWriter;
        writer.write_response_event(
            host,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::Auth,
                provider_signal: Some("invalid_api_key".into()),
                retry_after: None,
            }),
        )
    });
    assert_eq!(dropped, ["error.code"]);
    let writer = ResponsesWriter;
    let http_error = writer.write_error(host, 401, "authentication_error", "key refused");
    assert_eq!(http_error["error"]["type"], "authentication_error");
    assert_eq!(http_error["error"]["code"], "invalid_api_key");
    assert_eq!(http_error["error"]["message"], "key refused");
}

#[test]
fn responses_accumulator_refusal_ends_output_before_crossing_fragment() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStopReason, IrStreamEvent, IrTokenLogprob};
    let host = crate::codec::test_host::HOST;
    let citation = |title: &str| crate::codec::ir::IrCitation {
        url: Some("https://example.invalid".into()),
        title: Some(title.into()),
        start_index: Some(0),
        end_index: Some(1),
        ..Default::default()
    };
    let logprob = |token: &str| IrTokenLogprob {
        token: token.into(),
        logprob: -0.25,
        bytes: None,
        bytes_null: false,
        top: Vec::new(),
    };

    for case in [
        "text",
        "refusal",
        "tool",
        "reasoning",
        "summary",
        "signature",
        "citation",
        "logprob",
    ] {
        for overflow in [false, true] {
            let writer = ResponsesWriter;
            // Retained citation structs and finalized wire JSON both count toward the cap.
            const CAP: usize = 4096;
            writer.set_translate_cap(CAP);
            writer.write_response_events(
                host,
                &IrStreamEvent::MessageStart {
                    role: crate::codec::ir::IrRole::Assistant,
                    usage: None,
                    id: Some("resp_cap".into()),
                    created: Some(1),
                    model: Some("test".into()),
                },
            );
            let block = match case {
                "tool" => IrBlockMeta::ToolUse {
                    id: "call_cap".into(),
                    name: "lookup".into(),
                },
                "reasoning" | "signature" => IrBlockMeta::Thinking { kind: None },
                "summary" => IrBlockMeta::Thinking {
                    kind: Some(crate::codec::ir::IrThinkingKind::Summary),
                },
                _ => IrBlockMeta::Text,
            };
            writer.write_response_events(
                host,
                &IrStreamEvent::BlockStart {
                    index: 0,
                    block,
                    refusal: case == "refusal",
                },
            );
            let delta = |fragment: &str| match case {
                "tool" => IrDelta::InputJsonDelta(fragment.into()),
                "reasoning" | "summary" => IrDelta::ThinkingDelta(fragment.into()),
                "signature" => IrDelta::SignatureDelta(fragment.into()),
                "citation" => IrDelta::CitationsDelta(vec![citation(fragment)]),
                "logprob" => IrDelta::LogprobsDelta(vec![logprob(fragment)]),
                _ => IrDelta::TextDelta(fragment.into()),
            };
            let first_fragment = if case == "tool" { "{\"a\":" } else { "a" };
            let last_fragment = if case == "tool" { "}" } else { "c" };
            let first = writer.write_response_events(
                host,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: delta(first_fragment),
                },
            );
            assert!(!first.iter().any(|(name, _)| name == "error"), "{case}");
            let crossing = if overflow {
                let carried_overhead = match case {
                    "citation" => "https://example.invalid".len(),
                    "logprob" => std::mem::size_of::<IrTokenLogprob>(),
                    _ => 0,
                };

                "x".repeat(CAP - carried_overhead)
            } else if case == "tool" {
                "1".into()
            } else {
                "b".into()
            };
            let crossed = writer.write_response_events(
                host,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: delta(&crossing),
                },
            );
            if overflow {
                assert_eq!(crossed.len(), 1, "{case}: {crossed:?}");
                assert_eq!(
                    crossed[0].0, "error",
                    "{case}: crossing fragment must fail before output"
                );
                assert_eq!(
                    crossed[0].1["error"]["message"],
                    "Responses stream accumulation limit exceeded",
                    "{case}"
                );
            } else {
                assert!(!crossed.iter().any(|(name, _)| name == "error"), "{case}");
            }

            let writer = writer.clone();
            let later = writer.write_response_events(
                host,
                &IrStreamEvent::BlockDelta {
                    index: 0,
                    delta: delta(last_fragment),
                },
            );
            let closed = writer.write_response_events(host, &IrStreamEvent::BlockStop { index: 0 });
            let terminal = writer.write_response_events(
                host,
                &IrStreamEvent::MessageDelta {
                    stop_reason: Some(IrStopReason::EndTurn),
                    stop_sequence: None,
                    usage: crate::codec::ir::IrUsage {
                        input_tokens: 7,
                        output_tokens: 11,
                        ..Default::default()
                    },
                    stop_detail: None,
                },
            );
            if overflow {
                assert!(
                    later.is_empty() && closed.is_empty() && terminal.is_empty(),
                    "{case}: a failed stream must not emit later fragments or success: {later:?} {closed:?} {terminal:?}"
                );
            } else {
                assert!(!closed.is_empty(), "{case}: normal item must close");
                let response = &terminal
                    .iter()
                    .find(|(name, _)| name == EVT_RESPONSE_COMPLETED)
                    .expect("normal stream completes")
                    .1["response"];
                assert_eq!(response["usage"]["input_tokens"], 7);
                assert_eq!(response["usage"]["output_tokens"], 11);
                let item = &response["output"][0];
                match case {
                    "tool" => assert_eq!(item["arguments"], "{\"a\":1}"),
                    "reasoning" => assert_eq!(item["content"][0]["text"], "abc"),
                    "summary" => assert_eq!(item["summary"][0]["text"], "abc"),
                    "signature" => assert_eq!(item["encrypted_content"], "abc"),
                    "refusal" => assert_eq!(item["content"][0]["refusal"], "abc"),
                    "citation" => {
                        let annotations = item["content"][0]["annotations"].as_array().unwrap();
                        assert_eq!(
                            annotations
                                .iter()
                                .map(|a| a["title"].as_str().unwrap())
                                .collect::<Vec<_>>(),
                            ["a", "b", "c"]
                        );
                    }
                    "logprob" => {
                        let logprobs = item["content"][0]["logprobs"].as_array().unwrap();
                        assert_eq!(
                            logprobs
                                .iter()
                                .map(|lp| lp["token"].as_str().unwrap())
                                .collect::<Vec<_>>(),
                            ["a", "b", "c"]
                        );
                    }
                    _ => assert_eq!(item["content"][0]["text"], "abc"),
                }
            }
        }
    }
}

#[test]
fn responses_stream_cap_failure_keeps_reported_usage_and_suppresses_success() {
    let host = crate::codec::test_host::HOST;
    for overflow in [false, true] {
        // The finalized message envelope also consumes the retained-output budget.
        const CAP: usize = 1024;
        let crossing = "LONG".repeat(CAP);
        let mut stream = crate::codec::proto_stream::StreamTranslate::new("responses", "openai")
            .unwrap()
            .with_translate_cap(CAP);
        let mut output = Vec::new();
        for data in [
            serde_json::json!({"id":"chat_cap","model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"abc"},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_cap","choices":[{"index":0,"delta":{"content":if overflow { crossing.as_str() } else { "e" }},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_cap","choices":[{"index":0,"delta":{"content":"d"},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_cap","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            serde_json::json!({"id":"chat_cap","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":11,"total_tokens":18}}),
        ] {
            let frame = format!("data: {data}\n\n");
            output.extend(stream.feed(host, frame.as_bytes()));
        }
        output.extend(stream.finish(host));
        let output = String::from_utf8(output).unwrap();
        if overflow {
            assert!(
                output.contains("Responses stream accumulation limit exceeded"),
                "{output}"
            );
            assert_eq!(output.matches("event: error\n").count(), 1, "{output}");
            assert!(
                !output.contains("LONG"),
                "crossing delta was forwarded: {output}"
            );
            assert!(
                !output.contains("response.output_text.done")
                    && !output.contains("response.completed"),
                "retained prefix was finalized as success: {output}"
            );
            assert_eq!(
                stream.terminal_error(),
                Some("Responses stream accumulation limit exceeded")
            );
        } else {
            assert!(!output.contains("event: error\n"), "{output}");
            assert!(output.contains("response.completed"), "{output}");
            assert!(
                output.contains("\"text\":\"abced\""),
                "the successful terminal must preserve every fragment: {output}"
            );
            assert_eq!(stream.terminal_error(), None);
        }
        let usage = stream
            .usage()
            .expect("reported work survives a local encode failure");
        assert_eq!((usage.input_tokens, usage.output_tokens), (7, 11));
    }
}

#[test]
fn responses_writer_compacts_media_gaps_and_keeps_terminal_item_positions() {
    use crate::codec::ir::{IrBlockMeta, IrDelta, IrStopReason, IrStreamEvent, IrUsage};
    let writer = ResponsesWriter;
    let host = crate::codec::test_host::HOST;
    let mut frames = Vec::new();
    frames.extend(writer.write_response_events(
        host,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_first".into()),
            created: Some(1),
            model: Some("test".into()),
        },
    ));
    for event in [
        IrStreamEvent::BlockStart {
            index: 0,
            block: IrBlockMeta::Text,
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: IrDelta::TextDelta("before".into()),
        },
        IrStreamEvent::BlockStop { index: 0 },
        IrStreamEvent::BlockStart {
            index: 1,
            block: IrBlockMeta::Image,
            refusal: false,
        },
        IrStreamEvent::BlockStop { index: 1 },
        IrStreamEvent::BlockStart {
            index: 2,
            block: IrBlockMeta::Text,
            refusal: false,
        },
    ] {
        frames.extend(writer.write_response_events(host, &event));
    }

    let writer = writer.clone();
    for event in [
        IrStreamEvent::BlockDelta {
            index: 2,
            delta: IrDelta::TextDelta("after".into()),
        },
        IrStreamEvent::BlockStart {
            index: 3,
            block: IrBlockMeta::Image,
            refusal: false,
        },
        IrStreamEvent::BlockStop { index: 3 },
        IrStreamEvent::BlockStart {
            index: 4,
            block: IrBlockMeta::ToolUse {
                id: "call_x".into(),
                name: "lookup".into(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 4,
            delta: IrDelta::InputJsonDelta("{}".into()),
        },
        IrStreamEvent::BlockStop { index: 4 },
        IrStreamEvent::BlockStop { index: 2 },
        IrStreamEvent::MessageDelta {
            stop_reason: Some(IrStopReason::ToolUse),
            stop_sequence: None,
            usage: IrUsage {
                input_tokens: 3,
                output_tokens: 5,
                ..Default::default()
            },
            stop_detail: None,
        },
    ] {
        frames.extend(writer.write_response_events(host, &event));
    }
    let added: Vec<_> = frames
        .iter()
        .filter(|(name, _)| name == EVT_OUTPUT_ITEM_ADDED)
        .map(|(_, data)| data)
        .collect();
    assert_eq!(added.len(), 3);
    let terminal = &frames
        .iter()
        .find(|(name, _)| name == EVT_RESPONSE_COMPLETED)
        .unwrap()
        .1["response"];
    assert_eq!(terminal["output"].as_array().unwrap().len(), 3);
    for (ordinal, item) in added.iter().enumerate() {
        assert_eq!(item["output_index"], serde_json::json!(ordinal));
        let id = &item["item"]["id"];
        assert_eq!(&terminal["output"][ordinal]["id"], id);
        for (_, data) in &frames {
            if &data["item_id"] == id || &data["item"]["id"] == id {
                assert_eq!(data["output_index"], serde_json::json!(ordinal));
            }
        }
    }
    assert_eq!(terminal["output"][1]["content"][0]["text"], "after");
    assert_eq!(terminal["output"][2]["arguments"], "{}");
    assert_eq!(terminal["usage"]["output_tokens"], 5);
    writer.write_response_events(host, &IrStreamEvent::MessageStop);

    let writer = ResponsesWriter;
    writer.write_response_events(
        host,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: Some("resp_next".into()),
            created: Some(1),
            model: Some("test".into()),
        },
    );
    let next = writer.write_response_events(
        host,
        &IrStreamEvent::BlockStart {
            index: 2,
            block: IrBlockMeta::Text,
            refusal: false,
        },
    );
    assert_eq!(
        next[0].1["output_index"], 0,
        "new stream clears old mapping"
    );
}

#[test]
fn refusal_deltas_arrive_immediately_and_terminal_snapshots_do_not_repeat_them() {
    use crate::codec::ir::{IrDelta, IrStopReason, IrStreamEvent, StreamDecodeState};
    let mut state = StreamDecodeState::default();
    let normal = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "é "}),
        &mut state,
    );
    assert!(normal.iter().any(|event| matches!(
        event,
        IrStreamEvent::BlockStart {
            index: 0,
            refusal: false,
            ..
        }
    )));
    let annotation = reader_read_response_events(
        EVT_OUTPUT_TEXT_ANNOTATION_ADDED,
        &serde_json::json!({"output_index": 0, "annotation": {
            "type": "url_citation", "url": "https://example.com", "title": "source",
            "start_index": 0, "end_index": 1
        }}),
        &mut state,
    );
    assert!(annotation.iter().any(|event| matches!(event,
        IrStreamEvent::BlockDelta { index: 0, delta: IrDelta::CitationsDelta(citations) }
            if citations[0].start_index == Some(0) && citations[0].end_index == Some(1))));
    reader_read_response_events(
        EVT_CONTENT_PART_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    let mut emitted = Vec::new();
    for delta in ["I ", "can't"] {
        let events = reader_read_response_events(
            EVT_REFUSAL_DELTA,
            &serde_json::json!({"output_index": 1, "content_index": 0, "delta": delta}),
            &mut state,
        );
        assert!(events.iter().any(|event| matches!(event,
            IrStreamEvent::BlockDelta { index: 1, delta: IrDelta::TextDelta(text) } if text == delta)));
        emitted.extend(events);
    }
    let done = serde_json::json!({"output_index": 1, "content_index": 0, "refusal": "I can't"});
    emitted.extend(reader_read_response_events(
        EVT_REFUSAL_DONE,
        &done,
        &mut state,
    ));
    assert!(reader_read_response_events(EVT_REFUSAL_DONE, &done, &mut state).is_empty());
    for event_type in [EVT_CONTENT_PART_DONE, EVT_OUTPUT_ITEM_DONE] {
        assert!(reader_read_response_events(
            event_type,
            &serde_json::json!({"output_index": 1}),
            &mut state
        )
        .is_empty());
    }
    emitted.extend(reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({"response": {
            "status": "completed", "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "é "}]},
                {"type": "message", "content": [{"type": "refusal", "refusal": "I can't"}]},
                {"type": "message", "content": [{"type": "refusal", "refusal": "Another refusal"}]}
            ], "usage": {"input_tokens": 5, "output_tokens": 8}
        }}),
        &mut state,
    ));
    let text: Vec<(usize, &str)> = emitted
        .iter()
        .filter_map(|event| match event {
            IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::TextDelta(text),
            } => Some((*index, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(text, vec![(1, "I "), (1, "can't"), (2, "Another refusal")]);
    for idx in [1, 2] {
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event,
            IrStreamEvent::BlockStart { index, refusal: true, .. } if *index == idx))
                .count(),
            1
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event,
            IrStreamEvent::BlockStop { index } if *index == idx))
                .count(),
            1
        );
    }
    assert!(emitted.iter().any(|event| matches!(event,
        IrStreamEvent::MessageDelta { stop_reason: Some(IrStopReason::Refusal), usage, .. }
            if usage.input_tokens == 5 && usage.output_tokens == 8)));
}

#[test]
fn refusal_done_without_deltas_carries_text_once_and_bodyless_completion_keeps_refusal() {
    use crate::codec::ir::{IrDelta, IrStopReason, IrStreamEvent, StreamDecodeState};
    let mut state = StreamDecodeState::default();
    let done = serde_json::json!({"output_index": 0, "content_index": 0, "refusal": "No."});
    let events = reader_read_response_events(EVT_REFUSAL_DONE, &done, &mut state);
    assert!(matches!(events.as_slice(), [
        IrStreamEvent::BlockStart { index: 0, refusal: true, .. },
        IrStreamEvent::BlockDelta { index: 0, delta: IrDelta::TextDelta(text) },
        IrStreamEvent::BlockStop { index: 0 }
    ] if text == "No."));
    assert!(reader_read_response_events(EVT_REFUSAL_DONE, &done, &mut state).is_empty());
    let terminal =
        reader_read_response_events(EVT_RESPONSE_COMPLETED, &serde_json::json!({}), &mut state);
    assert!(terminal.iter().any(|event| matches!(
        event,
        IrStreamEvent::MessageDelta {
            stop_reason: Some(IrStopReason::Refusal),
            ..
        }
    )));
}

#[test]
fn refusal_deltas_survive_failed_and_incomplete_terminals_with_usage() {
    use crate::codec::ir::{IrDelta, IrStopReason, IrStreamEvent, StreamDecodeState};
    for (event_type, status, expected_stop) in [
        (EVT_RESPONSE_FAILED, "failed", IrStopReason::Error),
        (
            EVT_RESPONSE_INCOMPLETE,
            "incomplete",
            IrStopReason::MaxTokens,
        ),
    ] {
        let mut state = StreamDecodeState::default();
        let events = reader_read_response_events(
            EVT_REFUSAL_DELTA,
            &serde_json::json!({"output_index": 0, "delta": "No."}),
            &mut state,
        );
        assert!(events.iter().any(|event| matches!(event,
            IrStreamEvent::BlockDelta { delta: IrDelta::TextDelta(text), .. } if text == "No.")));
        let terminal = reader_read_response_events(
            event_type,
            &serde_json::json!({"response": {
                "status": status, "error": {"code": "server_error"},
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 9, "output_tokens": 4}
            }}),
            &mut state,
        );
        assert!(terminal
            .iter()
            .any(|event| matches!(event, IrStreamEvent::BlockStop { index: 0 })));
        assert!(terminal.iter().any(|event| matches!(event,
            IrStreamEvent::MessageDelta { stop_reason, usage, .. }
                if *stop_reason == Some(expected_stop) && usage.input_tokens == 9 && usage.output_tokens == 4)));
    }
}

#[test]
fn refusal_events_respect_existing_block_limit_and_tool_collision() {
    use crate::codec::ir::StreamDecodeState;
    let mut capped = StreamDecodeState::default();
    capped.open_tools.extend(0..MAX_OPEN_TOOLS);
    let mut collision = StreamDecodeState::default();
    collision.open_tools.insert(MAX_OUTPUT_INDEX);
    for state in [&mut capped, &mut collision] {
        for event_type in [EVT_REFUSAL_DELTA, EVT_REFUSAL_DONE] {
            let events = reader_read_response_events(
                event_type,
                &serde_json::json!({"output_index": MAX_OUTPUT_INDEX, "delta": "No.", "refusal": "No."}),
                state,
            );
            assert!(
                events.is_empty(),
                "no orphan or collision events: {events:?}"
            );
        }
        assert!(state.tool_ir_index.is_empty());
    }
}

/// An `input_file` content block (a legitimate Responses input shape) must not
/// silently vanish.
#[test]
fn responses_input_file_degrades_with_warn_not_silent_drop() {
    let body = serde_json::json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "before"},
                {"type": "input_file", "file_id": "file-x"},
                {"type": "input_text", "text": "after"}
            ]
        }]
    });

    let (_cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let ir = tracing::dispatcher::with_default(&subscriber, || {
        ResponsesReader.read_request(&body).expect("read_request")
    });

    assert_eq!(
        ir.messages[0].content.len(),
        3,
        "the unmodeled block must not vanish — it must degrade in place: {:?}",
        ir.messages[0].content
    );

    assert!(
        matches!(
            &ir.messages[0].content[1],
            crate::codec::ir::IrBlock::Media {
                kind: crate::codec::ir::IrMediaKind::Document,
                source: crate::codec::ir::IrImageSource::Vendor { value, .. },
                ..
            } if value["file_id"] == "file-x"
        ),
        "an input_file must be carried, not degraded: {:?}",
        ir.messages[0].content[1]
    );

    let (cap2, cap2_subscriber) = warnings();
    let sub2 = cap2_subscriber.clone();
    let ir2 = tracing::dispatcher::with_default(&sub2, || {
        ResponsesReader
            .read_request(&serde_json::json!({
                "model": "gpt-4.1",
                "input": [{"role": "user", "content": [
                    {"type": "input_text", "text": "before"},
                    {"type": "some_future_part", "whatever": 1},
                    {"type": "input_text", "text": "after"}
                ]}]
            }))
            .expect("read_request")
    });

    assert_eq!(
        ir2.messages[0].content.len(),
        2,
        "an unmodeled block is dropped, not replaced: {:?}",
        ir2.messages[0].content
    );
    assert!(
        !diagnostic_texts(&cap2)
            .iter()
            .any(|event| event.contains("some_future_part")),
        "the read is not a translate attempt: {:?}",
        diagnostic_texts(&cap2)
    );
}

/// LOW (lossless-by-target): the Responses create API models no `top_k`. A request carrying
/// `top_k` must NOT emit a `top_k` field (it would 400 a real `/v1/responses` call) — it is
/// dropped with a `warn!` (the drop-with-warn branch is exercised here).
#[test]
fn write_request_drops_top_k_with_warn() {
    let mk = |top_k: Option<u32>| crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        max_tokens: Some(64),
        top_p: Some(0.9),
        top_k,
        ..Default::default()
    };

    let writer = ResponsesWriter;

    let with = writer.write_request(crate::codec::test_host::HOST, &mk(Some(40)));
    assert!(
        with.get("top_k").is_none(),
        "the Responses body must NOT carry top_k (lossy-by-target): {with}"
    );

    assert_eq!(with.get("top_p").and_then(|v| v.as_f64()), Some(0.9));

    let without = writer.write_request(crate::codec::test_host::HOST, &mk(None));
    assert!(without.get("top_k").is_none());
}

#[test]
fn test_write_request() {
    let ir = crate::codec::ir::IrRequest {
        system: vec![crate::codec::ir::IrBlock::text(
            "You are helpful.".to_string(),
        )],
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "fc_1".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "SF"}),
                    cache_control: None,
                }],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Tool,
                content: vec![crate::codec::ir::IrBlock::ToolResult {
                    tool_use_id: "fc_1".to_string(),
                    content: vec![crate::codec::ir::IrBlock::text("sunny".to_string())],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        tools: vec![crate::codec::ir::IrTool {
            name: "get_weather".to_string(),
            description: Some("Get weather for a location".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            cache_control: None,
            hosted: None,
            strict: None,
        }],
        max_tokens: Some(1024),
        temperature: Some(0.7),
        ..Default::default()
    };

    let writer = ResponsesWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);

    assert_eq!(
        json.get("instructions").and_then(|v| v.as_str()),
        Some("You are helpful.")
    );

    let input = json
        .get("input")
        .and_then(|v| v.as_array())
        .expect("input should exist");

    let first_item = &input[0];
    assert_eq!(
        first_item.get("role").and_then(|r| r.as_str()),
        Some("user")
    );
    let content = first_item
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content should exist");
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0].get("type"),
        Some(&serde_json::json!("input_text")) // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("hi"));

    let func_call_item = input
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        .expect("should have function_call item");
    assert_eq!(
        func_call_item.get("name").and_then(|n| n.as_str()),
        Some("get_weather")
    );
    let args = func_call_item
        .get("arguments")
        .and_then(|a| a.as_str())
        .expect("arguments should exist");
    assert!(args.contains("SF") || args.contains("city"));

    let func_output_item = input
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call_output"))
        .expect("should have function_call_output item");
    assert_eq!(
        func_output_item.get("call_id").and_then(|c| c.as_str()),
        Some("fc_1")
    );
    let output = func_output_item
        .get("output")
        .and_then(|o| o.as_str())
        .expect("output should exist");
    assert_eq!(output, "sunny");

    let tools = json
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("tools should exist");
    assert_eq!(tools.len(), 1);
    let tool_obj = &tools[0];
    assert_eq!(tool_obj.get("type"), Some(&serde_json::json!("function")));
    assert_eq!(
        tool_obj.get("name").and_then(|n| n.as_str()),
        Some("get_weather")
    );
    assert!(
        tool_obj.get("function").is_none(),
        "tools should be flattened"
    );

    assert_eq!(
        json.get("max_output_tokens"),
        Some(&serde_json::json!(1024))
    );
    assert_eq!(json.get("temperature"), Some(&serde_json::json!(0.7)));
}

#[test]
fn test_read_request() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "instructions": "You are helpful.",
        "input": [
            {"role": "user", "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "What's the weather?"}]},
            {"role": "assistant", "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "Let me check that for you."}]},
            {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "fc_1", "name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
            {"type": "function_call_output", "call_id": "fc_1", "output": "Sunny, 72F"}
        ],
        "tools": [{"type": "function", "name": "get_weather", "description": "Get weather for a location", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}],
        "max_output_tokens": 1024,
        "temperature": 0.7
    });

    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("read_request should succeed");

    assert_eq!(ir.system.len(), 1);
    if let crate::codec::ir::IrBlock::Text { text, .. } = &ir.system[0] {
        assert_eq!(text, "You are helpful.");
    } else {
        panic!("system should be Text block");
    }

    assert_eq!(ir.messages.len(), 4);

    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    if let crate::codec::ir::IrBlock::Text { text, .. } = &ir.messages[0].content[0] {
        assert_eq!(text, "What's the weather?");
    } else {
        panic!("first message should be Text block");
    }

    assert_eq!(ir.max_tokens, Some(1024));
    assert_eq!(ir.temperature, Some(0.7_f64));

    assert_eq!(ir.tools.len(), 1);
    let tool = &ir.tools[0];
    assert_eq!(tool.name, "get_weather");
}

#[test]
fn test_read_request_bare_string_content_survives() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            // typed message arm, bare-string content
            {"type": ITEM_TYPE_MESSAGE, "role": "user", "content": "hello from typed"},
            // typed message arm, assistant bare-string content
            {"type": ITEM_TYPE_MESSAGE, "role": "assistant", "content": "typed assistant reply"},
            // untyped role-keyed fallback, bare-string content
            {"role": "user", "content": "hello from untyped"},
            {"role": "assistant", "content": "untyped assistant reply"}
        ]
    });

    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("read_request should succeed");

    assert_eq!(ir.messages.len(), 4, "no turn may be dropped");

    let expect_text =
        |msg: &crate::codec::ir::IrMessage, role: crate::codec::ir::IrRole, text: &str| {
            assert_eq!(msg.role, role);
            assert_eq!(msg.content.len(), 1);
            match &msg.content[0] {
                crate::codec::ir::IrBlock::Text { text: t, .. } => assert_eq!(t, text),
                other => panic!("expected Text block, got {other:?}"),
            }
        };

    expect_text(
        &ir.messages[0],
        crate::codec::ir::IrRole::User,
        "hello from typed",
    );
    expect_text(
        &ir.messages[1],
        crate::codec::ir::IrRole::Assistant,
        "typed assistant reply",
    );
    expect_text(
        &ir.messages[2],
        crate::codec::ir::IrRole::User,
        "hello from untyped",
    );
    expect_text(
        &ir.messages[3],
        crate::codec::ir::IrRole::Assistant,
        "untyped assistant reply",
    );
}

#[test]
fn test_roundtrip_identity() {
    let ir = crate::codec::ir::IrRequest {
        system: vec![crate::codec::ir::IrBlock::text(
            "You are helpful.".to_string(),
        )],
        messages: vec![
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::User,
                content: vec![crate::codec::ir::IrBlock::text("Hello!".to_string())],
            },
            crate::codec::ir::IrMessage {
                role: crate::codec::ir::IrRole::Assistant,
                content: vec![crate::codec::ir::IrBlock::text("Hi there!".to_string())],
            },
        ],
        max_tokens: Some(500),
        temperature: Some(0.7),
        ..Default::default()
    };

    let reader = ResponsesReader;
    let writer = ResponsesWriter;

    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let rt_ir = reader
        .read_request(&json)
        .expect("read round-trip should succeed");

    assert_eq!(ir, rt_ir);
}

#[test]
fn test_temperature_fidelity() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "test"}]}],
        "temperature": 0.7,
        "max_output_tokens": 1024
    });

    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("read_request should succeed");

    assert_eq!(ir.temperature, Some(0.7_f64));
}

#[test]
fn test_auth_headers() {
    let Some(crate::codec::protocol::EgressScheme::Static {
        families,
        own,
        passthrough,
    }) = crate::codec::openai_responses::DECL.egress_scheme
    else {
        panic!("responses declares a static credential scheme");
    };
    assert!(families.is_empty());
    assert_eq!(own, crate::codec::protocol::CredentialHeader::Bearer);
    assert_eq!(
        passthrough,
        crate::codec::protocol::CredentialHeader::Bearer
    );
    assert!(
        crate::codec::openai_responses::DECL
            .egress_auth_headers
            .is_none(),
        "no credential passes through the plane"
    );
}

#[test]
fn test_read_response_decode() {
    let json = serde_json::json!({
        "id": "resp_1",
        "object": OBJ_RESPONSE,
        "status": STATUS_COMPLETED,
        "output": [
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "The weather in SF is sunny."}]
            },
            {
                "type": ITEM_TYPE_FUNCTION_CALL,
                "call_id": "fc_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"SF\"}"
            }
        ],
        "usage": {"input_tokens": 50, "output_tokens": 25}
    });

    let reader = ResponsesReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");

    assert_eq!(resp.content.len(), 2);
    match &resp.content[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => {
            assert_eq!(text, "The weather in SF is sunny.")
        }
        _ => panic!("first block should be Text"),
    }
    match &resp.content[1] {
        crate::codec::ir::IrBlock::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(id, "fc_1");
            assert_eq!(name, "get_weather");
            assert_eq!(input.get("city").and_then(|v| v.as_str()), Some("SF"));
        }
        _ => panic!("second block should be ToolUse"),
    }

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::ToolUse)
    );
    assert_eq!(resp.usage.input_tokens, 50);
    assert_eq!(resp.usage.output_tokens, 25);
}

/// Lenience: a Responses `read_response` body with NO `usage` object must parse to a
/// zero IrUsage rather than hard-erroring with a ClientError — mirroring all five sibling readers
/// (openai_chat.rs, etc.).
#[test]
fn read_response_without_usage_zero_defaults_no_error() {
    let json = serde_json::json!({
        "id": "resp_no_usage",
        "object": OBJ_RESPONSE,
        "status": STATUS_COMPLETED,
        "output": [
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi"}]
            }
        ]
        // intentionally NO "usage" key
    });

    let resp = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("absent usage must parse to zero, not error");
    assert_eq!(resp.usage.input_tokens, 0);
    assert_eq!(resp.usage.output_tokens, 0);
    assert_eq!(resp.usage.cache_read_input_tokens, None);
}

#[test]
fn test_write_response_roundtrip_text_only() {
    let json = serde_json::json!({
        "id": "resp_abc123",
        "object": OBJ_RESPONSE,
        "created_at": 1_700_000_000_u64,
        "status": STATUS_COMPLETED,
        "model": "gpt-4o",
        "output": [
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "Hello world"}]
            }
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "error": serde_json::Value::Null
    });

    let reader = ResponsesReader;
    let writer = ResponsesWriter;

    let ir_resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read should succeed");
    let out = writer.write_response(crate::codec::test_host::HOST, &ir_resp);

    assert_eq!(out["id"], json["id"]);
    assert_eq!(out["object"], "response");
    assert_eq!(out["created_at"], json["created_at"]);
    assert_eq!(out["status"], "completed");
    assert_eq!(out["model"], "gpt-4o");

    assert_eq!(out["usage"]["input_tokens"], 10);
    assert_eq!(out["usage"]["output_tokens"], 5);
    assert_eq!(out["usage"]["total_tokens"], 15);
    assert!(out["error"].is_null());

    let item = &out["output"][0];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "assistant");
    assert_eq!(item["status"], "completed");
    let id = item["id"].as_str().expect("message item carries an id");
    assert!(
        id.starts_with("msg_") && id.len() > 4,
        "item id must be a native opaque msg_ token, got {id}"
    );
    let part = &item["content"][0];
    assert_eq!(part["type"], "output_text");
    assert_eq!(part["text"], "Hello world");
    assert!(
        part["annotations"].as_array().is_some_and(|a| a.is_empty()),
        "output_text part must carry annotations: [], got {part}"
    );
}

/// The NON-streaming `write_response` function_call
/// item must carry the item-level opaque `id` (`fc_…`, distinct from `call_id`) that the streaming
/// `output_item.done` emits, or a typed SDK reading `item.id` sees a missing field.
#[test]
fn test_write_response_function_call_item_has_native_id() {
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        id: Some("resp_x".to_string()),
        model: Some("gpt-4o".to_string()),
        created: Some(1_700_000_000),
        content: vec![crate::codec::ir::IrBlock::ToolUse {
            thought_signature: None,
            id: "call_abc".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "SF"}),
            cache_control: None,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        system_fingerprint: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    let fc = out["output"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["type"] == "function_call"))
        .expect("a function_call output item");
    assert_eq!(fc["call_id"], "call_abc", "call_id preserved");
    let id = fc["id"]
        .as_str()
        .expect("function_call item carries an item-level id");
    assert!(
        id.starts_with("fc_") && id.len() > 3,
        "function_call item id must be a native opaque fc_ token, got {id}"
    );
}

/// The NON-streaming `read_response` tool-use override must NOT clobber a
/// truncation reason.
#[test]
fn test_read_response_incomplete_with_function_call_keeps_max_tokens() {
    let json = serde_json::json!({
        "id": "resp_trunc",
        "object": OBJ_RESPONSE,
        "created_at": 1_700_000_000_u64,
        "status": STATUS_INCOMPLETE,
        "model": "gpt-4o",
        "incomplete_details": { "reason": INCOMPLETE_REASON_MAX_OUTPUT },
        "output": [
            {
                "type": ITEM_TYPE_FUNCTION_CALL,
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"SF\"}"
            }
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let reader = ResponsesReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read should succeed");

    assert!(
        resp.content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })),
        "the partial function_call must still be present as a ToolUse block"
    );

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::MaxTokens),
        "an incomplete (max_output_tokens) response must keep stop_reason=max_tokens, not be \
             promoted to tool_use just because a partial function_call survived"
    );
}

/// A native Responses refusal rides on a
/// `{type:"refusal", refusal:"..."}` content part with `status:"completed"`.
#[test]
fn read_response_refusal_part_survives_and_promotes_stop_reason() {
    let json = serde_json::json!({
        "id": "resp_refuse",
        "object": OBJ_RESPONSE,
        "created_at": 1_700_000_000_u64,
        "status": STATUS_COMPLETED,
        "model": "gpt-4o",
        "output": [
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [
                    { "type": "refusal", "refusal": "I can't help with that." }
                ]
            }
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let reader = ResponsesReader;
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read should succeed");

    assert!(
        resp.content.iter().any(|b| matches!(
            b,
            crate::codec::ir::IrBlock::Text { text, .. } if text == "I can't help with that."
        )),
        "the refusal text must survive as assistant content, not be silently dropped: {:?}",
        resp.content
    );

    assert_eq!(
        resp.stop_reason,
        Some(crate::codec::ir::IrStopReason::Refusal),
        "a completed response carrying a refusal part must surface stop_reason=refusal"
    );
}

/// `write_response` must build the `output` array in IR ENCOUNTER order so it
/// mirrors the streaming `drain_output_items` order.
#[test]
fn test_write_response_preserves_text_after_tool_order() {
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        id: Some("resp_order".to_string()),
        model: Some("gpt-4o".to_string()),
        created: Some(1_700_000_000),
        content: vec![
            crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            },
            crate::codec::ir::IrBlock::text("Here is the weather.".to_string()),
        ],
        stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        system_fingerprint: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    let arr = out["output"].as_array().expect("output is an array");
    assert_eq!(arr.len(), 2, "one item per non-empty block, in order");

    assert_eq!(
        arr[0]["type"],
        "function_call", // golden wire-contract literal (kept bare on purpose)
        "the tool block was first in IR content, so it must be output[0]"
    );
    assert_eq!(arr[0]["call_id"], "call_1");
    assert_eq!(
        arr[1]["type"],
        "message", // golden wire-contract literal (kept bare on purpose)
        "the text block came after the tool block, so it must NOT be forced to output[0]"
    );
    assert_eq!(arr[1]["content"][0]["text"], "Here is the weather.");
}

/// The native Responses error envelope an official SDK decodes: a JSON object whose `error`
#[test]
fn test_write_error_native_responses_envelope() {
    let writer = ResponsesWriter;
    let v = writer.write_error(
        crate::codec::test_host::HOST,
        404,
        "not_found",
        "model 'x' not found",
    );

    let serialized = serde_json::to_string(&v).expect("write_error output must serialize");
    let reparsed: serde_json::Value =
        serde_json::from_str(&serialized).expect("write_error output must be valid JSON");

    let err = reparsed.get("error").expect("error object present");
    assert_eq!(
        err.get("message").and_then(|m| m.as_str()),
        Some("model 'x' not found")
    );

    assert_eq!(
        err.get("type").and_then(|t| t.as_str()),
        Some("not_found_error")
    );

    assert!(err.get("code").is_some(), "code key must be present");
    assert!(err.get("param").is_some(), "param key must be present");
    assert!(err.get("code").unwrap().is_null());
    assert!(err.get("param").unwrap().is_null());
}

/// Each generic `kind` maps to the canonical Responses `error.type`; an unrecognized kind is
/// passed through verbatim (no catch-all swallowing of a precise upstream type).
#[test]
fn test_write_error_kind_mapping() {
    let writer = ResponsesWriter;

    for (kind, want) in [
        ("invalid_request", "invalid_request_error"),
        ("auth", "authentication_error"),
        ("forbidden", "permission_error"),
        ("not_found", "not_found_error"),
        ("rate_limit", "rate_limit_error"),
        ("server_error", "server_error"),
        ("billing", "insufficient_quota"),
        ("authentication_error", "authentication_error"),
        ("some_future_type", "some_future_type"),
    ] {
        let v = writer.write_error(crate::codec::test_host::HOST, 400, kind, "m");
        assert_eq!(
            v.get("error")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str()),
            Some(want),
            "kind {kind} should map to {want}"
        );
    }
}

/// Same-protocol passthrough: `read_response` captures the upstream `id`/`created_at`, and
/// `write_response` emits them verbatim — identity is preserved exactly, not regenerated.
#[test]
fn test_same_protocol_roundtrip_preserves_identity() {
    let json = serde_json::json!({
        "id": "resp_0123456789abcdef",
        "object": OBJ_RESPONSE,
        "created_at": 1_710_000_000_u64,
        "status": STATUS_COMPLETED,
        "model": "gpt-4o-2024-08-06",
        "output": [
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi"}]
            }
        ],
        "usage": {"input_tokens": 3, "output_tokens": 1}
    });

    let reader = ResponsesReader;
    let writer = ResponsesWriter;

    let ir = reader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read should succeed");
    assert_eq!(ir.id.as_deref(), Some("resp_0123456789abcdef"));
    assert_eq!(ir.created, Some(1_710_000_000));

    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("id").and_then(|i| i.as_str()),
        Some("resp_0123456789abcdef"),
        "id must be preserved verbatim"
    );
    assert_eq!(
        out.get("created_at").and_then(|c| c.as_u64()),
        Some(1_710_000_000),
        "created_at must be preserved verbatim"
    );
    assert_eq!(out.get("object").and_then(|o| o.as_str()), Some("response"));
}

/// The streaming start event captures the nested `response` identity for same-protocol
/// passthrough.
#[test]
fn test_stream_message_start_captures_identity() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_CREATED,
        &serde_json::json!({
            "response": {
                "id": "resp_streamid",
                "object": OBJ_RESPONSE,
                "created_at": 1_720_000_000_u64,
                "model": "gpt-4o",
                "status": STATUS_IN_PROGRESS
            }
        }),
        &mut state,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        crate::codec::ir::IrStreamEvent::MessageStart {
            id, created, model, ..
        } => {
            assert_eq!(id.as_deref(), Some("resp_streamid"));
            assert_eq!(*created, Some(1_720_000_000));
            assert_eq!(model.as_deref(), Some("gpt-4o"));
        }
        other => panic!("expected MessageStart, got {other:?}"),
    }
}

/// Cross-protocol: when the IR carries no identity (the backend supplied none), `write_response`
#[test]
fn test_cross_protocol_write_synthesizes_valid_id() {
    let writer = ResponsesWriter;
    let make_ir = || crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::text("answer".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: None,
        id: None,
        created: None,
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let out1 = writer.write_response(crate::codec::test_host::HOST, &make_ir());
    let id1 = out1
        .get("id")
        .and_then(|i| i.as_str())
        .expect("synthesized id present");
    assert!(
        id1.starts_with("resp_"), // golden wire-contract literal (kept bare on purpose)
        "synthesized id must use the resp_ prefix, got {id1}"
    );
    assert!(
        out1.get("created_at").and_then(|c| c.as_u64()).is_some(),
        "synthesized created_at must be present"
    );

    let out2 = writer.write_response(crate::codec::test_host::HOST, &make_ir());
    let id2 = out2.get("id").and_then(|i| i.as_str()).unwrap();
    assert_ne!(id1, id2, "successive synthesized ids must be unique");
}

#[test]
fn test_stream_fanout() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let events1 = reader_read_response_events(
        EVT_RESPONSE_CREATED,
        &serde_json::json!({"response": {"object": OBJ_RESPONSE, "status": STATUS_IN_PROGRESS}}),
        &mut state,
    );
    assert_eq!(events1.len(), 1);
    assert!(matches!(
        events1[0],
        crate::codec::ir::IrStreamEvent::MessageStart { .. }
    ));

    let events2 = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 1,
            "item": {"type": ITEM_TYPE_FUNCTION_CALL, "call_id":"fc_1","name":"get_weather"}
        }),
        &mut state,
    );
    assert_eq!(events2.len(), 1);
    assert!(matches!(
        events2[0],
        crate::codec::ir::IrStreamEvent::BlockStart { .. }
    ));

    let delta_json = |d: &str| serde_json::json!({"output_index": 0, "delta": d});
    let events3a = reader_read_response_events(EVT_OUTPUT_TEXT_DELTA, &delta_json("H"), &mut state);
    assert_eq!(events3a.len(), 2);
    assert!(matches!(
        events3a[0],
        crate::codec::ir::IrStreamEvent::BlockStart { .. }
    ));
    assert!(matches!(
        events3a[1],
        crate::codec::ir::IrStreamEvent::BlockDelta { .. }
    ));
    let events3b = reader_read_response_events(EVT_OUTPUT_TEXT_DELTA, &delta_json("i"), &mut state);
    assert_eq!(events3b.len(), 1);
    assert!(matches!(
        events3b[0],
        crate::codec::ir::IrStreamEvent::BlockDelta { .. }
    ));
    let events3c = reader_read_response_events(EVT_OUTPUT_TEXT_DELTA, &delta_json("!"), &mut state);
    assert_eq!(events3c.len(), 1);

    let events4 = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert_eq!(events4.len(), 1);
    assert!(matches!(
        events4[0],
        crate::codec::ir::IrStreamEvent::BlockStop { .. }
    ));

    let completed_json = serde_json::json!({
        "response": {
            "status": STATUS_COMPLETED,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }
    });
    let events5 = reader_read_response_events(EVT_RESPONSE_COMPLETED, &completed_json, &mut state);
    assert_eq!(events5.len(), 3);
    assert!(
        matches!(
            events5[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 1 }
        ),
        "still-open tool block at index 1 must be closed before MessageStop, got {:?}",
        events5[0]
    );
    assert!(matches!(
        events5[1],
        crate::codec::ir::IrStreamEvent::MessageDelta { .. }
    ));
    assert!(matches!(
        events5[2],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));

    let events6 = reader_read_response_events(
        "response.in_progress",
        &serde_json::json!({"response": {"object": OBJ_RESPONSE, "status": STATUS_IN_PROGRESS}}),
        &mut state,
    );
    assert_eq!(events6.len(), 0);

    let events7 = reader_read_response_events(
        "response.content_part.added",
        &serde_json::json!({}),
        &mut state,
    );
    assert_eq!(events7.len(), 0);
}

/// A terminal-only Responses refusal still needs the buffered-body fallback when no
/// refusal.delta or refusal.done events preceded response.completed.
#[test]
fn read_response_events_streamed_refusal_surfaces_text_and_refusal_stop_reason() {
    use crate::codec::ir::{IrDelta, IrStopReason, IrStreamEvent};
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let events = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({
            "response": {
                "object": OBJ_RESPONSE,
                "status": STATUS_COMPLETED,
                "output": [{
                    "type": ITEM_TYPE_MESSAGE,
                    "role": "assistant",
                    "content": [{ "type": "refusal", "refusal": "I can't help with that." }]
                }],
                "usage": { "input_tokens": 5, "output_tokens": 3 }
            }
        }),
        &mut state,
    );

    assert!(
            events.iter().any(|e| matches!(
                e,
                IrStreamEvent::BlockDelta { delta: IrDelta::TextDelta(t), .. } if t == "I can't help with that."
            )),
            "streamed refusal text must surface as a TextDelta; got {events:?}"
        );

    let stop = events.iter().find_map(|e| match e {
        IrStreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(
        stop,
        Some(Some(IrStopReason::Refusal)),
        "a streamed refusal must terminate with stop_reason=Refusal; got {events:?}"
    );

    let starts = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStart { .. }))
        .count();
    let stops = events
        .iter()
        .filter(|e| matches!(e, IrStreamEvent::BlockStop { .. }))
        .count();
    assert_eq!(
        (starts, stops),
        (1, 1),
        "one Start/Stop for the refusal block; got {events:?}"
    );
}

#[test]
fn test_write_response_event_blockdelta() {
    let writer = ResponsesWriter;

    let ev1 = crate::codec::ir::IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
    };
    let (etype1, payload1) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev1)
        .expect("should emit");
    assert_eq!(etype1, "response.output_text.delta");
    assert_eq!(payload1.get("delta").and_then(|d| d.as_str()), Some("hi"));

    let ev2 = crate::codec::ir::IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (etype2, payload2) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev2)
        .expect("should emit");
    assert_eq!(etype2, "response.completed");
    let resp_obj = payload2
        .get("response")
        .expect("payload should have response");
    assert_eq!(
        resp_obj.get("status"),
        Some(&serde_json::json!("completed")) // golden wire-contract literal (kept bare on purpose)
    );
}

fn reader_read_response_events(
    event_type: &str,
    data: &serde_json::Value,
    state: &mut crate::codec::ir::StreamDecodeState,
) -> Vec<crate::codec::ir::IrStreamEvent> {
    let reader = ResponsesReader;
    reader.read_response_events(crate::codec::test_host::HOST, event_type, data, state)
}

/// Regression: a text part arriving at a non-zero `output_index` must open AND write to the
/// same block index.
#[test]
fn test_text_delta_index_pairing_nonzero() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 2, "delta": "hello"}),
        &mut state,
    );
    assert_eq!(events.len(), 2, "expected lazy BlockStart + BlockDelta");
    let start_idx = match &events[0] {
        crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => *index,
        other => panic!("first event should be BlockStart, got {other:?}"),
    };
    let delta_idx = match &events[1] {
        crate::codec::ir::IrStreamEvent::BlockDelta { index, .. } => *index,
        other => panic!("second event should be BlockDelta, got {other:?}"),
    };
    assert_eq!(start_idx, 2, "BlockStart must use the wire output_index");
    assert_eq!(delta_idx, 2, "BlockDelta must use the wire output_index");
    assert_eq!(start_idx, delta_idx, "open/write indices must match");
}

/// Regression: an empty-delta keepalive chunk must produce no events, even when a text block is
/// already open.
#[test]
fn test_empty_delta_keepalive_emits_nothing() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let opened = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "x"}),
        &mut state,
    );
    assert_eq!(opened.len(), 2);
    assert!(state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));

    let keepalive = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": ""}),
        &mut state,
    );
    assert!(
        keepalive.is_empty(),
        "empty keepalive delta must not emit events, got {keepalive:?}"
    );

    let mut fresh = crate::codec::ir::StreamDecodeState::default();
    let pre = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": ""}),
        &mut fresh,
    );
    assert!(pre.is_empty());
    assert!(!fresh.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));
}

/// Regression: output_item.done must clear the open TEXT index so a subsequent text part can
/// lazily re-open its own block instead of silently reusing stale open state.
#[test]
fn test_done_clears_text_block_open() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let _ = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "a"}),
        &mut state,
    );
    assert!(state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));
    let done = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert_eq!(done.len(), 1);
    assert!(matches!(
        done[0],
        crate::codec::ir::IrStreamEvent::BlockStop { .. }
    ));
    assert!(
        !state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET),
        "done must clear the open text index"
    );

    let reopen = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 1, "delta": "b"}),
        &mut state,
    );
    assert_eq!(reopen.len(), 2);
    assert!(matches!(
        reopen[0],
        crate::codec::ir::IrStreamEvent::BlockStart { index: 1, .. }
    ));
}

/// A bodyless `response.incomplete` terminal event (no nested `response`
#[test]
fn test_bodyless_incomplete_is_not_end_turn() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events =
        reader_read_response_events(EVT_RESPONSE_INCOMPLETE, &serde_json::json!({}), &mut state);

    let delta_stop = events
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .expect("bodyless incomplete must still emit a MessageDelta");
    assert_eq!(
        *delta_stop, None,
        "bodyless incomplete must surface stop_reason None, not a fabricated end_turn"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, crate::codec::ir::IrStreamEvent::MessageStop)),
        "stream must still terminate with MessageStop"
    );

    let mut s2 = crate::codec::ir::StreamDecodeState::default();
    let completed =
        reader_read_response_events(EVT_RESPONSE_COMPLETED, &serde_json::json!({}), &mut s2);
    let completed_reason = completed
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .expect("bodyless completed must still emit a MessageDelta");
    assert_eq!(
        *completed_reason,
        Some(crate::codec::ir::IrStopReason::EndTurn),
        "bodyless completed must map to end_turn"
    );
}

#[test]
fn test_stream_completed_with_function_call_is_tool_use_not_end_turn() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({
            "response": {
                "status": STATUS_COMPLETED,
                "output": [
                    { "type": ITEM_TYPE_FUNCTION_CALL, "id": "fc_1", "call_id": "call_1",
                      "name": "get_weather", "arguments": "{}" }
                ]
            }
        }),
        &mut state,
    );
    let stop = events
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .expect("terminal MessageDelta");
    assert_eq!(
        *stop,
        Some(crate::codec::ir::IrStopReason::ToolUse),
        "a streamed completed response containing a function_call must be tool_use, not end_turn"
    );
}

#[test]
fn test_stream_completed_without_function_call_stays_end_turn() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({
            "response": {
                "status": STATUS_COMPLETED,
                "output": [
                    { "type": ITEM_TYPE_MESSAGE, "role": "assistant",
                      "content": [{ "type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi", "annotations": [] }] }
                ]
            }
        }),
        &mut state,
    );
    let stop = events
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .expect("terminal MessageDelta");
    assert_eq!(
        *stop,
        Some(crate::codec::ir::IrStopReason::EndTurn),
        "text-only completed stays end_turn"
    );
}

/// A terminal event arriving while a content block is STILL OPEN must
/// close that block (BlockStop) before MessageStop.
#[test]
fn test_terminal_closes_open_blocks_balanced() {
    fn assert_balanced(etype: &str, data: serde_json::Value) {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let mut all: Vec<crate::codec::ir::IrStreamEvent> = Vec::new();

        all.extend(reader_read_response_events(
            EVT_OUTPUT_TEXT_DELTA,
            &serde_json::json!({"output_index": 0, "delta": "partial"}),
            &mut state,
        ));

        all.extend(reader_read_response_events(
            EVT_OUTPUT_ITEM_ADDED,
            &serde_json::json!({
                "output_index": 1,
                "item": {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "c1", "name": "f"}
            }),
            &mut state,
        ));

        assert!(
            state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET),
            "{etype}: text block should be open"
        );
        assert!(
            state.open_tools.contains(&1),
            "{etype}: tool block should be open"
        );

        all.extend(reader_read_response_events(etype, &data, &mut state));

        use std::collections::BTreeMap;
        let mut starts: BTreeMap<usize, usize> = BTreeMap::new();
        let mut stops: BTreeMap<usize, usize> = BTreeMap::new();
        for ev in &all {
            match ev {
                crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => {
                    *starts.entry(*index).or_insert(0) += 1;
                }
                crate::codec::ir::IrStreamEvent::BlockStop { index } => {
                    *stops.entry(*index).or_insert(0) += 1;
                }
                _ => {}
            }
        }
        assert_eq!(
                starts, stops,
                "{etype}: BlockStart/BlockStop counts must balance per index (starts={starts:?} stops={stops:?})"
            );

        assert_eq!(
            starts.get(&0).copied(),
            Some(1),
            "{etype}: text opened once"
        );
        assert_eq!(stops.get(&0).copied(), Some(1), "{etype}: text closed once");
        assert_eq!(
            starts.get(&1).copied(),
            Some(1),
            "{etype}: tool opened once"
        );
        assert_eq!(stops.get(&1).copied(), Some(1), "{etype}: tool closed once");

        assert!(
            state.open_tools.is_empty(),
            "{etype}: open_tools must be drained after the terminal event"
        );

        let msg_stop_pos = all
            .iter()
            .position(|e| matches!(e, crate::codec::ir::IrStreamEvent::MessageStop))
            .expect("must emit MessageStop");
        let last_block_stop = all
            .iter()
            .rposition(|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStop { .. }))
            .expect("must emit BlockStop");
        assert!(
            last_block_stop < msg_stop_pos,
            "{etype}: all BlockStop must precede MessageStop"
        );
    }

    assert_balanced(EVT_RESPONSE_COMPLETED, serde_json::json!({}));
    assert_balanced(EVT_RESPONSE_INCOMPLETE, serde_json::json!({}));
    assert_balanced(EVT_RESPONSE_FAILED, serde_json::json!({}));

    assert_balanced(
        EVT_RESPONSE_COMPLETED,
        serde_json::json!({"response": {"status": STATUS_COMPLETED}}),
    );

    assert_balanced(
        EVT_RESPONSE_INCOMPLETE,
        serde_json::json!({
            "response": {"status": STATUS_INCOMPLETE, "incomplete_details": {"reason": INCOMPLETE_REASON_MAX_OUTPUT}}
        }),
    );

    assert_balanced(
        EVT_RESPONSE_FAILED,
        serde_json::json!({"response": {"status": STATUS_FAILED, "error": {"code": ERR_TYPE_SERVER_ERROR}}}),
    );
}

/// Regression: content_part.done is also a terminal-of-part signal and must close its block.
#[test]
fn test_content_part_done_closes_block() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let _ = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "a"}),
        &mut state,
    );
    let done = reader_read_response_events(
        "response.content_part.done",
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert_eq!(done.len(), 1);
    assert!(matches!(
        done[0],
        crate::codec::ir::IrStreamEvent::BlockStop { .. }
    ));
    assert!(!state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));
}

/// Regression: a minimal `response.completed` lacking a nested `response` object must still
/// terminate the stream with MessageDelta + MessageStop, not leave it hanging.
#[test]
fn test_completed_without_response_object_terminates() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events =
        reader_read_response_events(EVT_RESPONSE_COMPLETED, &serde_json::json!({}), &mut state);
    assert_eq!(events.len(), 2, "must emit MessageDelta + MessageStop");
    assert!(matches!(
        events[0],
        crate::codec::ir::IrStreamEvent::MessageDelta { .. }
    ));
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));
}

/// Regression: same for `response.incomplete` with no nested response object.
#[test]
fn test_incomplete_without_response_object_terminates() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events =
        reader_read_response_events(EVT_RESPONSE_INCOMPLETE, &serde_json::json!({}), &mut state);
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        crate::codec::ir::IrStreamEvent::MessageDelta { .. }
    ));
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));

    let mut s2 = crate::codec::ir::StreamDecodeState::default();
    let failed = reader_read_response_events(EVT_RESPONSE_FAILED, &serde_json::json!({}), &mut s2);
    assert_eq!(failed.len(), 2);
    assert!(matches!(
        failed[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));
}

/// Regression: an input item carrying BOTH a `type` and a `role` must be processed exactly once
/// (by the type arm), not duplicated by the role-keyed fallback.
#[test]
fn test_typed_item_with_role_not_duplicated() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {
                "type": CONTENT_TYPE_OUTPUT_TEXT,
                "role": "assistant",
                "text": "hello",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "DUPLICATE"}]
            }
        ]
    });
    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("read_request should succeed");

    assert_eq!(ir.messages.len(), 1, "typed+role item must not duplicate");
    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::Assistant);
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => assert_eq!(text, "hello"),
        other => panic!("expected text turn, got {other:?}"),
    }
}

/// Regression: an assistant turn that is PURELY a tool call must emit a flat `function_call`
#[test]
fn test_tool_only_assistant_turn_no_empty_message_wrapper() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::ToolUse {
                thought_signature: None,
                id: "fc_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
                cache_control: None,
            }],
        }],
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let input = json
        .get("input")
        .and_then(|v| v.as_array())
        .expect("input should exist");

    assert_eq!(
        input.len(),
        1,
        "tool-only turn must not emit an empty message wrapper, got {input:?}"
    );
    assert_eq!(
        input[0].get("type").and_then(|t| t.as_str()),
        Some("function_call") // golden wire-contract literal (kept bare on purpose)
    );

    for item in input {
        if item.get("role").is_some() {
            let content = item.get("content").and_then(|c| c.as_array());
            assert!(
                content.map(|c| !c.is_empty()).unwrap_or(true),
                "no assistant message item may have empty content"
            );
        }
    }
}

/// Regression: an assistant turn carrying BOTH text and a tool call must emit the assistant
/// `message` (with the text) FIRST, then the flat `function_call` item AFTER it — preserving
/// the conversation order the assistant produced.
#[test]
fn test_assistant_text_then_tool_call_order() {
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![
                crate::codec::ir::IrBlock::text("Let me check.".to_string()),
                crate::codec::ir::IrBlock::ToolUse {
                    thought_signature: None,
                    id: "fc_9".to_string(),
                    name: "lookup".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                },
            ],
        }],
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let input = json
        .get("input")
        .and_then(|v| v.as_array())
        .expect("input should exist");
    assert_eq!(
        input.len(),
        2,
        "expected message + function_call, got {input:?}"
    );

    assert_eq!(
        input[0].get("role").and_then(|r| r.as_str()),
        Some("assistant")
    );
    let content = input[0]
        .get("content")
        .and_then(|c| c.as_array())
        .expect("message content");
    assert_eq!(
        content[0].get("text").and_then(|t| t.as_str()),
        Some("Let me check.")
    );

    assert_eq!(
        input[1].get("type").and_then(|t| t.as_str()),
        Some("function_call") // golden wire-contract literal (kept bare on purpose)
    );
    assert_eq!(
        input[1].get("call_id").and_then(|c| c.as_str()),
        Some("fc_9")
    );
}

/// Regression: a streaming `response.failed` (status=="failed") must surface an
/// IrStreamEvent::Error followed by MessageStop, NOT a successful end_turn MessageDelta that
/// would mask the failure from a downstream client.
#[test]
fn test_stream_failed_status_emits_error_not_end_turn() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_FAILED,
        &serde_json::json!({
            "response": {
                "status": STATUS_FAILED,
                "error": {"code": ERR_TYPE_SERVER_ERROR, "type": ERR_TYPE_SERVER_ERROR}
            }
        }),
        &mut state,
    );
    assert_eq!(
        events.len(),
        2,
        "expected Error + MessageStop, got {events:?}"
    );
    match &events[0] {
        crate::codec::ir::IrStreamEvent::Error(err) => {
            assert_eq!(err.provider_signal.as_deref(), Some("server_error"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, crate::codec::ir::IrStreamEvent::MessageDelta { .. })),
        "failed stream must not emit a MessageDelta"
    );
}

/// A streamed `response.failed` must classify the IrError by the captured
/// provider signal, not a hardcoded ServerError.
#[test]
fn test_stream_failed_invalid_api_key_classifies_as_auth() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_FAILED,
        &serde_json::json!({
            "response": {
                "status": STATUS_FAILED,
                "error": {"code": "invalid_api_key", "type": ERR_TYPE_AUTHENTICATION}
            }
        }),
        &mut state,
    );
    match &events[0] {
        crate::codec::ir::IrStreamEvent::Error(err) => {
            assert_eq!(
                err.class,
                StatusClass::Auth,
                "invalid_api_key mid-stream must classify as Auth, not ServerError"
            );

            assert_eq!(err.provider_signal.as_deref(), Some("invalid_api_key"));
        }
        other => panic!("expected Error, got {other:?}"),
    }

    assert_eq!(
        class_for_response_failed("invalid_api_key"),
        StatusClass::Auth
    );
    assert_eq!(
        class_for_response_failed(ERR_TYPE_AUTHENTICATION),
        StatusClass::Auth
    );
    assert_eq!(
        class_for_response_failed(ERR_CODE_RATE_LIMIT),
        StatusClass::RateLimit
    );
    assert_eq!(
        class_for_response_failed(ERR_TYPE_INSUFFICIENT_QUOTA),
        StatusClass::Billing
    );
    assert_eq!(
        class_for_response_failed(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH),
        StatusClass::ContextLength
    );
    assert_eq!(
        class_for_response_failed(ERR_CODE_STRING_ABOVE_MAX),
        StatusClass::ContextLength
    );
    assert_eq!(
        class_for_response_failed(ERR_TYPE_SERVER_ERROR),
        StatusClass::ServerError
    );
    assert_eq!(
        class_for_response_failed(ERR_TYPE_OVERLOADED),
        StatusClass::ServerError
    );

    assert_eq!(
        class_for_response_failed("response_failed"),
        StatusClass::ServerError
    );
}

/// The NON-streaming `read_response` `status:"failed"` path must
/// also classify by the captured provider signal rather than hardcoding ServerError.
#[test]
fn test_read_response_failed_body_classifies_by_signal() {
    let reader = ResponsesReader;
    let err = reader
            .read_response(crate::codec::test_host::HOST, &serde_json::json!({
                "status": STATUS_FAILED,
                "output": [],
                "error": {"code": crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH, "type": ERR_TYPE_INVALID_REQUEST}
            }))
            .expect_err("failed body must surface an IrError");
    assert_eq!(
        err.class,
        StatusClass::ContextLength,
        "context_length_exceeded failed body must classify as ContextLength, not ServerError"
    );
    assert_eq!(
        err.provider_signal.as_deref(),
        Some("context_length_exceeded") // golden wire-contract literal (kept bare on purpose)
    );

    let err_auth = reader
        .read_response(
            crate::codec::test_host::HOST,
            &serde_json::json!({
                "status": STATUS_FAILED,
                "output": [],
                "error": {"code": "invalid_api_key", "type": ERR_TYPE_AUTHENTICATION}
            }),
        )
        .expect_err("failed body must surface an IrError");
    assert_eq!(err_auth.class, StatusClass::Auth);
}

/// Regression: an unknown terminal status must not be decoded as a successful end_turn; its
/// stop_reason is None (terminal, but no success claim).
#[test]
fn test_stream_unknown_status_not_end_turn() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({"response": {"status": "some_future_status"}}),
        &mut state,
    );
    assert_eq!(events.len(), 2);
    match &events[0] {
        crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => {
            assert_eq!(*stop_reason, None, "unknown status must not claim end_turn");
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));
}

/// Regression: a streaming `incomplete` status with NO `incomplete_details` must NOT decode to
/// `stop_reason = Some("end_turn")` — that masks the truncation as a clean completion.
#[test]
fn test_stream_incomplete_without_details_is_none() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_INCOMPLETE,
        &serde_json::json!({"response": {"status": STATUS_INCOMPLETE}}),
        &mut state,
    );
    assert_eq!(events.len(), 2);
    match &events[0] {
        crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => {
            assert_eq!(
                *stop_reason, None,
                "incomplete with no details must not claim end_turn"
            );
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_INCOMPLETE,
        &serde_json::json!({
            "response": {"status": STATUS_INCOMPLETE, "incomplete_details": {}}
        }),
        &mut state,
    );
    assert_eq!(events.len(), 2);
    match &events[0] {
        crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => {
            assert_eq!(
                *stop_reason, None,
                "incomplete_details without a reason must not claim end_turn"
            );
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        crate::codec::ir::IrStreamEvent::MessageStop
    ));

    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader_read_response_events(
        EVT_RESPONSE_INCOMPLETE,
        &serde_json::json!({
            "response": {
                "status": STATUS_INCOMPLETE,
                "incomplete_details": {"reason": INCOMPLETE_REASON_MAX_OUTPUT}
            }
        }),
        &mut state,
    );
    match &events[0] {
        crate::codec::ir::IrStreamEvent::MessageDelta { stop_reason, .. } => {
            assert_eq!(
                stop_reason,
                &Some(crate::codec::ir::IrStopReason::MaxTokens)
            );
        }
        other => panic!("expected MessageDelta, got {other:?}"),
    }
}

/// a multi-block ToolResult must concatenate its text fragments with NO separator. A `.join(" ")`
#[test]
fn write_request_tool_result_multi_text_concatenates_without_separator() {
    fn text_block(s: &str) -> crate::codec::ir::IrBlock {
        crate::codec::ir::IrBlock::text(s.to_string())
    }
    let writer = ResponsesWriter;
    let multi = || crate::codec::ir::IrBlock::ToolResult {
        tool_use_id: "call_1".to_string(),
        content: vec![text_block("AAA"), text_block("BBB")],
        is_error: false,
        cache_control: None,
    };

    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Tool,
            content: vec![multi()],
        }],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let item = out["input"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["type"] == "function_call_output"))
        .expect("a function_call_output item (Tool role)");
    assert_eq!(
        item["output"], "AAABBB",
        "Tool-role multi-text ToolResult must concatenate with NO separator, got {}",
        item["output"]
    );

    let req = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![multi()],
        }],
        ..Default::default()
    };
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    let item = out["input"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["type"] == "function_call_output"))
        .expect("a function_call_output item (Assistant role)");
    assert_eq!(
        item["output"], "AAABBB",
        "Assistant-role multi-text ToolResult must concatenate with NO separator, got {}",
        item["output"]
    );
}

/// Regression: the streaming error event nests the error inside the `response` object the
/// official SDK streaming decoder reads via `event.response`, and the error object is the
/// Responses-native `ResponseError` shape `{code, message}` with a NON-NULL `code` enum — NOT
/// the Chat-Completions `{message, type, code:null, param:null}` envelope.
#[test]
fn test_write_error_stream_event_full_shape() {
    let writer = ResponsesWriter;
    let ev = crate::codec::ir::IrStreamEvent::Error(IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("boom".to_string()),
        retry_after: None,
    });
    let (etype, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error event should emit");
    assert_eq!(etype, "response.failed");

    assert!(
        payload.get("error").is_none(),
        "error must not be top-level: {payload}"
    );
    let resp = payload.get("response").expect("response object present");
    assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("failed"));
    let err = resp.get("error").expect("nested error object present");
    assert_eq!(err.get("message").and_then(|m| m.as_str()), Some("boom"));

    assert_eq!(
        err.get("code").and_then(|c| c.as_str()),
        Some("server_error")
    );
    assert!(
        !err.as_object().unwrap().contains_key("type"),
        "Responses ResponseError carries no `type` field: {err}"
    );
    assert!(
        !err.as_object().unwrap().contains_key("param"),
        "Responses ResponseError carries no `param` field: {err}"
    );
}

/// Regression: an unknown/unmapped stop_reason must map to a `completed` status (not `failed`),
/// so a future IR reason that did not signal an error is not misclassified as a failure.
#[test]
fn test_unknown_stop_reason_maps_to_completed() {
    let writer = ResponsesWriter;

    let ev = crate::codec::ir::IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::Refusal),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (etype, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("should emit");
    assert_eq!(etype, "response.completed");
    assert_eq!(
        payload
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str()),
        Some("completed"), // golden wire-contract literal (kept bare on purpose)
        "unknown stop_reason must map to completed in stream"
    );

    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::text("ok".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::Refusal),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: None,
        id: Some("resp_x".to_string()),
        created: Some(1),
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out.get("status").and_then(|s| s.as_str()),
        Some("completed"), // golden wire-contract literal (kept bare on purpose)
        "unknown stop_reason must map to completed in write_response"
    );
}

/// Regression: malformed function_call arguments must be preserved as the raw string, not
/// dropped to Null (mirrors the OpenAI reader).
#[test]
fn test_malformed_function_call_args_preserved() {
    let reader = ResponsesReader;

    let req_json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "fc_1", "name": "f", "arguments": "not-json{"}
        ]
    });
    let ir = reader.read_request(&req_json).expect("read_request ok");
    let tool_use = ir
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { input, .. } => Some(input),
            _ => None,
        })
        .expect("tool use present");
    assert_eq!(
        tool_use.as_str(),
        Some("not-json{"),
        "malformed args must be preserved as raw string, not Null"
    );

    let resp_json = serde_json::json!({
        "id": "resp_1",
        "status": STATUS_COMPLETED,
        "output": [
            {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "fc_2", "name": "g", "arguments": "broken]"}
        ],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let resp = reader
        .read_response(crate::codec::test_host::HOST, &resp_json)
        .expect("read_response ok");
    match &resp.content[0] {
        crate::codec::ir::IrBlock::ToolUse { input, .. } => {
            assert_eq!(input.as_str(), Some("broken]"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// Regression: a base64 `input_image` data URI of the canonical single-`;` shape
/// (`data:image/png;base64,<payload>`) must parse the FULL payload, not drop it to "".
#[test]
fn test_input_image_base64_payload_preserved() {
    let payload = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let url = format!("data:image/png;base64,{payload}");

    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"type": "input_image", "image_url": url}
        ]
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&json).expect("read_request ok");
    assert_eq!(ir.messages.len(), 1);
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
            ..
        } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, payload, "full base64 payload must be preserved");
        }
        other => panic!("expected Image, got {other:?}"),
    }

    let block = serde_json::json!({"type": "input_image", "image_url": url});
    match responses_block(&block).expect("responses_block ok") {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Base64 { media_type, data },
            ..
        } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, payload);
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

/// Regression: a base64 `input_image` must survive a same-protocol read -> write -> read
/// round-trip with its payload intact (the writer emits `data:<mime>;base64,<payload>` which the
/// reader must parse back to the identical pair).
#[test]
fn test_input_image_roundtrip_lossless() {
    let payload = "QUJDMTIzKz0=";
    let media_type = "image/jpeg";
    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Base64 {
                    media_type: media_type.to_string(),
                    data: payload.to_string(),
                },
                cache_control: None,
                detail: None,
            }],
        }],
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let reader = ResponsesReader;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let rt = reader.read_request(&json).expect("read round-trip ok");
    match &rt.messages[0].content[0] {
        crate::codec::ir::IrBlock::Image {
            source:
                crate::codec::ir::IrImageSource::Base64 {
                    media_type: mt,
                    data,
                },
            ..
        } => {
            assert_eq!(mt, media_type);
            assert_eq!(data, payload, "round-trip must not corrupt the payload");
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

/// Regression: a non-data (https) image URL must be stored verbatim under the `image_url`
#[test]
fn test_input_image_https_url_sentinel_roundtrip() {
    let url = "https://example.com/cat.png";
    let block = serde_json::json!({"type": "input_image", "image_url": url});
    let stored_url = match responses_block(&block).expect("responses_block ok") {
        crate::codec::ir::IrBlock::Image {
            source: crate::codec::ir::IrImageSource::Url(url),
            ..
        } => url,
        other => panic!("expected Image(Url), got {other:?}"),
    };
    assert_eq!(
        stored_url, url,
        "URL must be stored verbatim as the typed Url source"
    );
    assert!(
        !stored_url.starts_with("// note"),
        "must not embed a human comment in the payload"
    );

    let ir = crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Url(stored_url),
                cache_control: None,
                detail: None,
            }],
        }],
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let json = writer.write_request(crate::codec::test_host::HOST, &ir);
    let emitted = json["input"][0]["content"][0]["image_url"]
        .as_str()
        .expect("image_url present");
    assert_eq!(emitted, url, "writer must emit the original URL verbatim");
}

/// Regression: `write_request` must emit the `stream` field (a modeled key excluded from
/// `extra`); omitting it answers a `stream: true` request non-streaming and stalls SSE.
#[test]
fn test_write_request_emits_stream() {
    let make = |stream: bool| crate::codec::ir::IrRequest {
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        }],
        stream,
        ..Default::default()
    };
    let writer = ResponsesWriter;
    assert_eq!(
        writer
            .write_request(crate::codec::test_host::HOST, &make(true))
            .get("stream"),
        Some(&serde_json::json!(true)),
        "stream: true must be emitted"
    );
    assert_eq!(
        writer
            .write_request(crate::codec::test_host::HOST, &make(false))
            .get("stream"),
        Some(&serde_json::json!(false)),
        "stream: false must be emitted explicitly"
    );
}

/// Regression: a typed `{"type":"message","role":...,"content":[...]}` input item (the official
/// SDK conversation-turn shape) must be read, not silently dropped.
#[test]
fn test_typed_message_item_read() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"type": ITEM_TYPE_MESSAGE, "role": "user",
             "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "hello typed"}]},
            {"type": ITEM_TYPE_MESSAGE, "role": "assistant",
             "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi back"}]}
        ]
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&json).expect("read_request ok");
    assert_eq!(
        ir.messages.len(),
        2,
        "both typed message turns must be read"
    );
    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => assert_eq!(text, "hello typed"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(ir.messages[1].role, crate::codec::ir::IrRole::Assistant);
    match &ir.messages[1].content[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => assert_eq!(text, "hi back"),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Regression: the streaming `response.created` event must carry `id`/`created_at`/`status`
#[test]
fn test_message_start_emits_identity() {
    let writer = ResponsesWriter;

    let ev = crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: Some("resp_streamid".to_string()),
        created: Some(1_720_000_000),
        model: Some("gpt-4o".to_string()),
    };
    let (etype, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("should emit");
    assert_eq!(etype, "response.created");
    let resp = payload.get("response").expect("response object");
    assert_eq!(
        resp.get("id").and_then(|i| i.as_str()),
        Some("resp_streamid")
    );
    assert_eq!(
        resp.get("created_at").and_then(|c| c.as_u64()),
        Some(1_720_000_000)
    );
    assert_eq!(resp.get("model").and_then(|m| m.as_str()), Some("gpt-4o"));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("in_progress") // golden wire-contract literal (kept bare on purpose)
    );

    let ev2 = crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let (_, payload2) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev2)
        .expect("should emit");
    let resp2 = payload2.get("response").expect("response object");
    let id = resp2
        .get("id")
        .and_then(|i| i.as_str())
        .expect("synthesized id present");
    assert!(
        id.starts_with("resp_"), // golden wire-contract literal (kept bare on purpose)
        "synthesized id must use resp_ prefix, got {id}"
    );
    assert!(
        resp2.get("created_at").and_then(|c| c.as_u64()).is_some(),
        "synthesized created_at must be present"
    );

    assert_eq!(
        resp2.get("model").and_then(|m| m.as_str()),
        Some(DEFAULT_MODEL),
        "absent model must fall back to DEFAULT_MODEL, not be omitted"
    );
}

/// Regression: synthesized response ids stay distinct even across many calls in the same second
/// (the old `timestamp << 24 ^ counter` folding collided once the counter advanced by 2^24).
#[test]
fn test_synthesize_response_id_unique() {
    let n = 1000;
    let ids: std::collections::HashSet<String> = (0..n)
        .map(|_| synthesize_response_id(crate::codec::test_host::HOST).expect("test entropy"))
        .collect();
    assert_eq!(
        ids.len(),
        n,
        "all synthesized ids in a burst must be unique"
    );
    assert!(ids.iter().all(|id| id.starts_with("resp_")));
}

/// `synth_token<const N>` documents and now ENFORCES a
/// `N >= 11` floor via a compile-time `const _: () = assert!(N >= 11, ...)` evaluated per
/// monomorphization.
#[test]
fn test_synth_token_meets_minimum_width() {
    const MIN_TOKEN_LEN: usize = 11;

    const { assert!(ITEM_ID_TOKEN_LEN >= MIN_TOKEN_LEN) };
    const { assert!(RESPONSE_ID_TOKEN_LEN >= MIN_TOKEN_LEN) };

    let resp_id = synthesize_response_id(crate::codec::test_host::HOST).expect("test entropy");
    let resp_suffix = resp_id
        .strip_prefix("resp_")
        .expect("synthesized response id uses resp_ prefix");
    assert!(
        resp_suffix.len() >= MIN_TOKEN_LEN,
        "synthesized resp_ suffix must be >= {MIN_TOKEN_LEN} base62 chars, got {} ({resp_id})",
        resp_suffix.len()
    );

    let item_id = synthesize_item_id(crate::codec::test_host::HOST, "msg").expect("test entropy");
    let item_suffix = item_id
        .strip_prefix("msg_")
        .expect("synthesized item id uses the given prefix");
    assert!(
        item_suffix.len() >= MIN_TOKEN_LEN,
        "synthesized item suffix must be >= {MIN_TOKEN_LEN} base62 chars, got {} ({item_id})",
        item_suffix.len()
    );
}

/// The streaming `response.failed` terminal event
/// (emitted from an IR `Error`) must carry the native non-error skeleton — specifically a
/// present-but-empty `output` array (REQUIRED by the SDK's typed `Response`), never omitting it.
#[test]
fn test_response_failed_carries_empty_output_skeleton() {
    let writer = ResponsesWriter;
    let (etype, failed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("boom".to_string()),
                retry_after: None,
            }),
        )
        .expect("Error emits response.failed");
    assert_eq!(etype, "response.failed");
    let resp = failed
        .get("response")
        .expect("response.failed wraps an inner response object");
    let output = resp
        .get("output")
        .expect("response.failed inner response must carry output, not omit it");
    assert!(
        output.as_array().is_some_and(|a| a.is_empty()),
        "response.failed output must be a present-but-empty array, got {output}"
    );

    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("failed"), // golden wire-contract literal (kept bare on purpose)
        "response.failed inner status must be \"failed\""
    );
    assert_eq!(
        resp.get("object").and_then(|o| o.as_str()),
        Some("response"), // golden wire-contract literal (kept bare on purpose)
        "response.failed inner object must be \"response\""
    );
    assert!(
        resp.get("id").and_then(|i| i.as_str()).is_some(),
        "response.failed inner response must carry an id"
    );
    assert!(
        resp.get("error").and_then(|e| e.as_object()).is_some(),
        "response.failed inner response must carry the error object"
    );
}

/// A top-level `metadata` object is carried by the typed slot (IR-03) and re-emitted by
/// `write_request`.
#[test]
fn test_metadata_round_trips_through_the_slot() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "hi"}]}],
        "metadata": {"trace_id": "abc-123", "team": "billing"}
    });
    let reader = ResponsesReader;
    let writer = ResponsesWriter;

    let ir = reader.read_request(&json).expect("read_request ok");

    assert_eq!(
        ir.metadata,
        Some(vec![
            ("team".to_string(), "billing".to_string()),
            ("trace_id".to_string(), "abc-123".to_string()),
        ]),
        "metadata must land in the typed slot, not be dropped"
    );
    assert!(!ir.extra.contains_key("metadata"));

    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("metadata"),
        Some(&serde_json::json!({"trace_id": "abc-123", "team": "billing"})),
        "metadata must be forwarded to the upstream Responses backend"
    );
}

/// The opening `response.created`
#[test]
fn test_message_start_skeleton_carries_usage_output_error() {
    let writer = ResponsesWriter;
    let ev = crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let (etype, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("should emit");
    assert_eq!(etype, "response.created");
    let resp = payload.get("response").expect("response object present");

    assert!(
        resp.get("usage").is_some(),
        "usage key must be present on the opening chunk: {resp}"
    );
    assert_eq!(
        resp["usage"],
        serde_json::json!({
            "input_tokens": 0,
            "input_tokens_details": { "cached_tokens": 0, "cache_write_tokens": 0 },
            "output_tokens": 0,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": 0,
        }),
        "usage must be a zeroed ResponseUsage object (no tokens yet) at stream start: {resp}"
    );

    assert_eq!(
        resp.get("output"),
        Some(&serde_json::json!([])),
        "output must be present as an empty array"
    );
    assert!(
        resp.get("error").map(|e| e.is_null()).unwrap_or(false),
        "error key must be present and null at stream start"
    );
}

/// A role-only item (no `type`) must still be processed via the role fallback.
#[test]
fn test_role_only_item_still_processed() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"role": "user", "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "hi there"}]}
        ]
    });
    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("read_request should succeed");
    assert_eq!(ir.messages.len(), 1);
    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    match &ir.messages[0].content[0] {
        crate::codec::ir::IrBlock::Text { text, .. } => assert_eq!(text, "hi there"),
        other => panic!("expected text turn, got {other:?}"),
    }
}

/// An array `input` must be iterated without the prior
/// `is_array()` + `.as_array().unwrap()` pattern.
#[test]
fn test_read_request_array_input_no_unwrap() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"type": CONTENT_TYPE_INPUT_TEXT, "text": "hello"},
            {"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "world"}
        ]
    });
    let reader = ResponsesReader;
    let ir = reader
        .read_request(&json)
        .expect("array input should decode");
    assert_eq!(ir.messages.len(), 2);
    assert_eq!(ir.messages[0].role, crate::codec::ir::IrRole::User);
    assert_eq!(ir.messages[1].role, crate::codec::ir::IrRole::Assistant);
}

/// A `response.failed` terminal event with NO nested `response`
#[test]
fn test_failed_event_without_body_surfaces_error() {
    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let data = serde_json::json!({});
    let events = reader.read_response_events(
        crate::codec::test_host::HOST,
        EVT_RESPONSE_FAILED,
        &data,
        &mut state,
    );

    assert_eq!(
        events.len(),
        2,
        "expected Error + MessageStop, got {events:?}"
    );
    match &events[0] {
        IrStreamEvent::Error(err) => {
            assert_eq!(err.class, StatusClass::ServerError);
            assert_eq!(err.provider_signal.as_deref(), Some("response_failed"));
        }
        other => panic!("expected Error first, got {other:?}"),
    }
    assert!(
        matches!(events[1], IrStreamEvent::MessageStop),
        "expected MessageStop, got {:?}",
        events[1]
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, IrStreamEvent::MessageDelta { .. })),
        "a bodyless failed event must not emit a success MessageDelta"
    );
}

/// A `response.completed`/`response.incomplete` terminal event with no nested `response` object
/// must still terminate the stream with a success MessageDelta + MessageStop (must NOT become an
/// Error — only `response.failed` does).
#[test]
fn test_completed_event_without_body_emits_end_turn() {
    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let data = serde_json::json!({});
    let events = reader.read_response_events(
        crate::codec::test_host::HOST,
        EVT_RESPONSE_COMPLETED,
        &data,
        &mut state,
    );

    assert_eq!(events.len(), 2, "expected MessageDelta + MessageStop");
    match &events[0] {
        IrStreamEvent::MessageDelta { stop_reason, .. } => {
            assert_eq!(stop_reason, &Some(crate::codec::ir::IrStopReason::EndTurn));
        }
        other => panic!("expected MessageDelta first, got {other:?}"),
    }
    assert!(matches!(events[1], IrStreamEvent::MessageStop));
    assert!(
        !events.iter().any(|e| matches!(e, IrStreamEvent::Error(_))),
        "a bodyless completed event must not emit an Error"
    );
}

/// The writer's `IrStreamEvent::Error` arm must emit a
/// `response.failed` event whose error lives inside a `response` object (the shape the official
/// SDK streaming decoder reads via `event.response`), with a synthesized `resp_` id and
/// `status: "failed"` — NOT a top-level `{"error":{...}}`.
#[test]
fn test_error_event_wraps_in_response_object() {
    let writer = ResponsesWriter;
    let ev = IrStreamEvent::Error(IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("overloaded".to_string()),
        retry_after: None,
    });
    let (etype, payload) = writer
        .write_response_event(crate::codec::test_host::HOST, &ev)
        .expect("error event should emit");
    assert_eq!(etype, "response.failed");

    assert!(
        payload.get("error").is_none(),
        "error must be nested under response, not top-level: {payload}"
    );
    let resp = payload
        .get("response")
        .expect("payload must carry a `response` object");
    let id = resp
        .get("id")
        .and_then(|i| i.as_str())
        .expect("synthesized resp_ id present");
    assert!(
        id.starts_with("resp_"), // golden wire-contract literal (kept bare on purpose)
        "synthesized id must use resp_ prefix, got {id}"
    );
    assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("failed"));
    let error = resp.get("error").expect("nested error object");
    assert_eq!(
        error.get("message").and_then(|m| m.as_str()),
        Some("overloaded")
    );

    assert_eq!(
        error.get("code").and_then(|c| c.as_str()),
        Some("server_error")
    );
    assert!(
        !error.as_object().unwrap().contains_key("type"),
        "Responses ResponseError carries no `type` field: {error}"
    );
    assert!(
        !error.as_object().unwrap().contains_key("param"),
        "Responses ResponseError carries no `param` field: {error}"
    );
}

/// EVERY emitted
#[test]
fn test_every_stream_event_carries_top_level_type() {
    let writer = ResponsesWriter;
    let usage = || crate::codec::ir::IrUsage {
        input_tokens: 1,
        output_tokens: 1,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    let events = vec![
        IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
        IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::ToolUse {
                id: "fc_1".to_string(),
                name: "f".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("{}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 0 },
        IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage(),
            stop_detail: None,
        },
        IrStreamEvent::Error(IrError {
            class: StatusClass::ServerError,
            provider_signal: Some("boom".to_string()),
            retry_after: None,
        }),
    ];

    for ev in &events {
        let (event_name, payload) = writer
            .write_response_event(crate::codec::test_host::HOST, ev)
            .unwrap_or_else(|| panic!("event {ev:?} must emit a body"));
        assert_eq!(
            payload.get("type").and_then(|t| t.as_str()),
            Some(event_name.as_str()),
            "event {event_name} body must carry top-level \"type\" == event name, got {payload}"
        );
    }
}

/// A full single-stream sequence of emitted Responses events. The events go through the writer in
/// the order `StreamTranslate::feed` would emit them.
fn usage_fixture() -> crate::codec::ir::IrUsage {
    crate::codec::ir::IrUsage {
        input_tokens: 1,
        output_tokens: 1,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    }
}

/// EVERY emitted Responses SSE event must carry a top-level
/// `sequence_number` that is monotonic from 0 within a single stream.
#[test]
fn test_sequence_number_monotonic_from_zero() {
    let writer = ResponsesWriter;

    let stream = vec![
        IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("Hel".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("lo".to_string()),
        },
        IrStreamEvent::BlockStop { index: 0 },
        IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage_fixture(),
            stop_detail: None,
        },
    ];

    let mut seqs = Vec::new();
    for ev in &stream {
        if let Some((_, payload)) = writer.write_response_event(crate::codec::test_host::HOST, ev) {
            let n = payload
                .get("sequence_number")
                .and_then(|s| s.as_u64())
                .unwrap_or_else(|| {
                    panic!("every emitted event must carry sequence_number: {payload}")
                });
            seqs.push(n);
        }
    }

    assert_eq!(
        seqs,
        vec![0, 1, 2, 3],
        "sequence_number must be 0..N monotonic within the stream, got {seqs:?}"
    );
}

/// Regression: a SECOND stream must restart its `sequence_number` from 0 — the counter is
/// per-stream, not per-process, so one stream's numbering never bleeds into the next on the same
/// worker.
#[test]
fn test_sequence_number_resets_per_stream() {
    let writer = ResponsesWriter;
    let start = || IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let delta = || IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("x".to_string()),
    };

    let (_, a0) = writer
        .write_response_event(crate::codec::test_host::HOST, &start())
        .expect("emit");
    let (_, a1) = writer
        .write_response_event(crate::codec::test_host::HOST, &delta())
        .expect("emit");
    assert_eq!(a0.get("sequence_number").and_then(|s| s.as_u64()), Some(0));
    assert_eq!(a1.get("sequence_number").and_then(|s| s.as_u64()), Some(1));

    let writer_b = ResponsesWriter;
    let (_, b0) = writer_b
        .write_response_event(crate::codec::test_host::HOST, &start())
        .expect("emit");
    let (_, b1) = writer_b
        .write_response_event(crate::codec::test_host::HOST, &delta())
        .expect("emit");
    assert_eq!(
        b0.get("sequence_number").and_then(|s| s.as_u64()),
        Some(0),
        "a new stream's response.created must reset sequence_number to 0"
    );
    assert_eq!(b1.get("sequence_number").and_then(|s| s.as_u64()), Some(1));
}

/// Regression: every writer arm that produces a body carries `sequence_number`, not just the
/// deltas.
#[test]
fn test_every_stream_event_carries_sequence_number() {
    let writer = ResponsesWriter;
    let events = vec![
        IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
        IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::ToolUse {
                id: "call_1".to_string(),
                name: "f".to_string(),
            },
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("{}".to_string()),
        },
        IrStreamEvent::BlockStop { index: 0 },
        IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage_fixture(),
            stop_detail: None,
        },
        IrStreamEvent::Error(IrError {
            class: StatusClass::ServerError,
            provider_signal: Some("boom".to_string()),
            retry_after: None,
        }),
    ];

    for ev in &events {
        let (event_name, payload) = writer
            .write_response_event(crate::codec::test_host::HOST, ev)
            .unwrap_or_else(|| panic!("event {ev:?} must emit a body"));
        assert!(
            payload
                .get("sequence_number")
                .map(|s| s.is_u64())
                .unwrap_or(false),
            "event {event_name} body must carry a u64 sequence_number, got {payload}"
        );
    }
}

/// Regression: `response.output_text.delta` must carry `item_id` and `content_index` (native
/// shape), and the `output_item.added` for a function call must carry `item_id`.
#[test]
fn test_delta_and_item_added_carry_item_id_and_content_index() {
    let writer = ResponsesWriter;

    let (_, text) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 2,
                delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
            },
        )
        .expect("emit");
    let text_item = text
        .get("item_id")
        .and_then(|i| i.as_str())
        .expect("output_text.delta must carry item_id");
    assert!(
        text_item.starts_with("msg_"), // golden wire-contract literal (kept bare on purpose)
        "text delta item_id must be a msg_ id, got {text_item}"
    );
    assert_eq!(
        text.get("content_index").and_then(|c| c.as_u64()),
        Some(0),
        "output_text.delta must carry content_index"
    );

    let (_, added) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 1,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_9".to_string(),
                    name: "lookup".to_string(),
                },
                refusal: false,
            },
        )
        .expect("emit");
    let added_item = added
        .get("item_id")
        .and_then(|i| i.as_str())
        .expect("output_item.added must carry item_id");
    assert!(
        added_item.starts_with("fc_"), // golden wire-contract literal (kept bare on purpose)
        "function_call item_id must be an fc_ id, got {added_item}"
    );

    assert_eq!(
        added
            .get("item")
            .and_then(|i| i.get("id"))
            .and_then(|i| i.as_str()),
        Some(added_item),
        "nested item.id must equal the top-level item_id"
    );

    let (_, args) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 1,
                delta: crate::codec::ir::IrDelta::InputJsonDelta("{\"q\":1}".to_string()),
            },
        )
        .expect("emit");
    assert_eq!(
        args.get("item_id").and_then(|i| i.as_str()),
        Some(added_item),
        "arguments delta item_id must match the item's added item_id (stable per index)"
    );
}

/// The `sequence_number` counter is PER-STREAM INSTANCE state,
/// not thread-local.
#[test]
fn test_sequence_number_is_per_instance_not_thread_local() {
    let stream_a = ResponsesWriter;
    let stream_b = ResponsesWriter;
    let start = || IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let delta = || IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("x".to_string()),
    };
    let seq = |opt: Option<(String, serde_json::Value)>| {
        opt.expect("emit")
            .1
            .get("sequence_number")
            .and_then(|s| s.as_u64())
            .expect("sequence_number present")
    };

    let a0 = seq(stream_a.write_response_event(crate::codec::test_host::HOST, &start()));
    let b0 = seq(stream_b.write_response_event(crate::codec::test_host::HOST, &start()));
    let a1 = seq(stream_a.write_response_event(crate::codec::test_host::HOST, &delta()));
    let b1 = seq(stream_b.write_response_event(crate::codec::test_host::HOST, &delta()));
    let a2 = seq(stream_a.write_response_event(crate::codec::test_host::HOST, &delta()));
    let b2 = seq(stream_b.write_response_event(crate::codec::test_host::HOST, &delta()));

    assert_eq!(
        (a0, a1, a2),
        (0, 1, 2),
        "stream A must stay monotonic-from-0 despite stream B interleaving"
    );
    assert_eq!(
        (b0, b1, b2),
        (0, 1, 2),
        "stream B must stay monotonic-from-0 independent of stream A"
    );
}

/// `response.output_item.done` must carry a stable `item_id`
#[test]
fn test_output_item_done_carries_matching_item_id_and_item() {
    let writer = ResponsesWriter;

    let (_, added) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 3,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_x".to_string(),
                    name: "f".to_string(),
                },
                refusal: false,
            },
        )
        .expect("added emits");
    let added_id = added
        .get("item_id")
        .and_then(|i| i.as_str())
        .expect("added carries item_id")
        .to_string();

    let frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 3 },
    );
    assert_eq!(
        frames
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "response.function_call_arguments.done",
            "response.output_item.done"
        ]
    );
    let done = &frames[1].1;
    assert_eq!(frames[0].1["item_id"], done["item_id"]);
    assert_eq!(frames[0].1["output_index"], added["output_index"]);
    assert_eq!(done["output_index"], added["output_index"]);
    assert_eq!(
        done.get("item_id").and_then(|i| i.as_str()),
        Some(added_id.as_str()),
        "output_item.done item_id must match the output_item.added at the same index"
    );

    let item = done
        .get("item")
        .and_then(|i| i.as_object())
        .expect("output_item.done must carry an item object");
    assert_eq!(
        item.get("type").and_then(|t| t.as_str()),
        Some("function_call"), // golden wire-contract literal (kept bare on purpose)
        "the done item must be typed"
    );
    assert_eq!(
        item.get("id").and_then(|i| i.as_str()),
        Some(added_id.as_str()),
        "the done item.id must equal the item_id"
    );
}

/// The in-band `response.failed` error object is the
/// Responses-native `ResponseError` shape `{code, message}` with a NON-NULL `code` enum — NOT
/// the Chat-Completions `{message, type, code:null, param:null}` envelope.
#[test]
fn test_response_failed_uses_native_responseerror_shape() {
    let writer = ResponsesWriter;

    let (etype, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("rate_limit_exceeded".to_string()),
                retry_after: None,
            }),
        )
        .expect("emit");
    assert_eq!(etype, "response.failed");
    let error = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.as_object())
        .expect("response.error object present");
    assert_eq!(
        error.get("code").and_then(|c| c.as_str()),
        Some("rate_limit_exceeded"), // golden wire-contract literal (kept bare on purpose)
        "error.code must be the non-null Responses error enum"
    );
    assert_eq!(
        error.get("message").and_then(|m| m.as_str()),
        Some("rate_limit_exceeded") // golden wire-contract literal (kept bare on purpose)
    );
    assert!(
        !error.contains_key("type"),
        "Responses ResponseError carries no `type` field"
    );
    assert!(
        !error.contains_key("param"),
        "Responses ResponseError carries no `param` field"
    );

    let (_, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: None,
                retry_after: None,
            }),
        )
        .expect("emit");
    let code = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str());
    assert_eq!(
        code,
        Some("server_error"), // golden wire-contract literal (kept bare on purpose)
        "error.code must default to server_error, never null"
    );
}

/// On a cross-protocol / transport-abort path `provider_signal` carries
/// a HUMAN sentence (e.g. `STREAM_ABORT_DETAIL`), NOT a Responses code enum. `error.code` must be
/// DERIVED from the class (a valid enum an SDK can switch on), while `error.message` keeps the
/// human text — the old code forwarded the sentence as the `code` enum, breaking typed SDKs.
#[test]
fn test_response_failed_code_is_enum_even_for_human_provider_signal() {
    let writer = ResponsesWriter;
    let (_, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some(crate::codec::framework::STREAM_ABORT_DETAIL.to_string()),
                retry_after: None,
            }),
        )
        .expect("emit");
    let error = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.as_object())
        .expect("error object");
    assert_eq!(
        error.get("code").and_then(|c| c.as_str()),
        Some("server_error"),
        "a human provider_signal must NOT leak as the code enum — derive it from the class"
    );
    assert_eq!(
        error.get("message").and_then(|m| m.as_str()),
        Some(crate::codec::framework::STREAM_ABORT_DETAIL),
        "the human text stays in `message`"
    );

    let (_, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::Auth,
                provider_signal: Some("connection reset by peer".to_string()),
                retry_after: None,
            }),
        )
        .expect("emit");
    assert_eq!(
        payload
            .pointer("/response/error/code")
            .and_then(|c| c.as_str()),
        Some(ERR_TYPE_SERVER_ERROR)
    );
}

/// A TEXT part must be bracketed inside a `message` output item AND its `output_text` content part.
#[test]
fn test_text_block_emits_message_item_lifecycle() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );

    let added_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );
    let added_types: Vec<&str> = added_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        added_types,
        vec!["response.output_item.added", "response.content_part.added"], // golden wire-contract literals (kept bare on purpose)
        "text BlockStart must open the message item THEN its output_text content part"
    );
    let added = &added_frames[0].1;
    assert_eq!(added["item"]["type"], "message");
    assert_eq!(added["item"]["role"], "assistant");
    let added_item_id = added["item_id"]
        .as_str()
        .expect("item_id present")
        .to_string();
    assert!(added_item_id.starts_with("msg_"), "text item id is msg_…");

    let part_added = &added_frames[1].1;
    assert_eq!(part_added["item_id"].as_str(), Some(added_item_id.as_str()));
    assert_eq!(part_added["content_index"], 0);
    assert_eq!(part_added["part"]["type"], "output_text");
    assert_eq!(part_added["part"]["text"], "");

    let _ = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
    );

    let done_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    let done_types: Vec<&str> = done_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        done_types,
        vec![
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done"
        ], // golden wire-contract literals (kept bare on purpose)
        "text BlockStop must close output_text → content_part → output_item in order"
    );

    let text_done = &done_frames[0].1;
    assert_eq!(text_done["text"], "hi");
    assert_eq!(text_done["content_index"], 0);
    assert_eq!(text_done["item_id"].as_str(), Some(added_item_id.as_str()));

    let part_done = &done_frames[1].1;
    assert_eq!(part_done["part"]["type"], "output_text");
    assert_eq!(part_done["part"]["text"], "hi");
    assert_eq!(part_done["item_id"].as_str(), Some(added_item_id.as_str()));

    let done = &done_frames[2].1;
    assert_eq!(done["item"]["type"], "message");
    assert_eq!(
        done["item_id"].as_str(),
        Some(added_item_id.as_str()),
        "done item_id matches the added item_id (added→done correlation)"
    );

    assert!(
        writer
            .write_response_events(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 0 }
            )
            .is_empty(),
        "a repeated BlockStop for a closed text index must not re-emit any lifecycle frame"
    );
}

/// An interleaved tool+text stream closes the tool index with a
/// `function_call` done and the text index with a `message` done — each with its own typed
/// item, never cross-typed.
#[test]
fn test_tool_and_text_block_stop_emit_correctly_typed_done() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );

    let _ = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_1".to_string(),
                    name: "f".to_string(),
                },
                refusal: false,
            },
        )
        .expect("tool added emits");
    let _ = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockStart {
                index: 1,
                block: crate::codec::ir::IrBlockMeta::Text,
                refusal: false,
            },
        )
        .expect("text added emits");
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 1,
            delta: crate::codec::ir::IrDelta::TextDelta("hi".to_string()),
        },
    );

    let tool_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    let tool_types: Vec<&str> = tool_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        tool_types,
        vec![
            "response.function_call_arguments.done",
            "response.output_item.done"
        ],
        "tool arguments finalize before the completed item"
    );
    assert_eq!(tool_frames[1].1["item"]["type"], "function_call");
    assert_eq!(tool_frames[0].1["item_id"], tool_frames[1].1["item_id"]);

    let text_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 1 },
    );
    let text_types: Vec<&str> = text_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        text_types,
        vec![
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done"
        ], // golden wire-contract literals (kept bare on purpose)
        "text BlockStop closes output_text → content_part → output_item"
    );

    assert_eq!(text_frames[2].1["item"]["type"], "message");

    assert!(
        writer
            .write_response_events(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStop { index: 0 }
            )
            .is_empty(),
        "a repeated BlockStop for a closed tool index must not re-emit output_item.done"
    );
}

/// The terminal `response.completed` event's inner `response`
#[test]
fn test_completed_event_carries_id_and_created_at() {
    let writer = ResponsesWriter;
    let (etype, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("emit");
    assert_eq!(etype, "response.completed");
    let resp = payload
        .get("response")
        .and_then(|r| r.as_object())
        .expect("response object present");
    let id = resp
        .get("id")
        .and_then(|i| i.as_str())
        .expect("response.completed must carry response.id");
    assert!(
        id.starts_with("resp_"), // golden wire-contract literal (kept bare on purpose)
        "synthesized id must be a resp_ id, got {id}"
    );
    assert!(
        resp.get("created_at").and_then(|c| c.as_u64()).is_some(),
        "response.completed must carry an integer created_at"
    );
}

/// `write_error` must emit `code:"invalid_api_key"` for an
/// authentication failure (mirrors `openai_chat.rs` `write_error_emits_invalid_api_key_code_for_auth_failure`).
#[test]
fn write_error_emits_invalid_api_key_code_for_auth_failure() {
    let writer = ResponsesWriter;
    for kind in ["authentication", "authentication_error", "auth"] {
        let body = writer.write_error(crate::codec::test_host::HOST, 401, kind, "bad key");
        assert_eq!(
            body["error"]["type"],
            serde_json::json!("authentication_error"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind} must map to authentication_error"
        );
        assert_eq!(
            body["error"]["code"],
            serde_json::json!("invalid_api_key"),
            "auth failure (kind {kind}) must carry code=invalid_api_key, not null"
        );
    }
}

/// Non-auth, non-quota error kinds keep `code:null` — the
/// native shape when no machine-readable code applies — so only the auth and quota paths are
/// special-cased.
#[test]
fn write_error_keeps_null_code_for_non_auth_errors() {
    let writer = ResponsesWriter;
    for kind in [
        "invalid_request",
        "permission",
        "not_found",
        "rate_limit",
        "server_error",
    ] {
        let body = writer.write_error(crate::codec::test_host::HOST, 400, kind, "msg");
        assert_eq!(
            body["error"]["code"],
            serde_json::Value::Null,
            "non-auth/non-quota kind {kind} must keep code=null"
        );
    }
}

/// The over-quota path carries a populated machine-readable
/// `code` — native OpenAI/Responses emits `{"type":"insufficient_quota","code":"insufficient_quota"}`
/// — so a `code:null` here (the old behavior) would be a fingerprintable divergence.
#[test]
fn write_error_insufficient_quota_keeps_type_and_sets_code() {
    let writer = ResponsesWriter;
    for kind in ["insufficient_quota", "billing"] {
        let body = writer.write_error(crate::codec::test_host::HOST, 429, kind, "over quota");
        assert_eq!(
            body["error"]["type"],
            serde_json::json!("insufficient_quota"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind} maps to the native insufficient_quota type"
        );
        assert_eq!(
            body["error"]["code"],
            serde_json::json!("insufficient_quota"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind} must carry code=insufficient_quota, not null"
        );
    }
}

/// A native text item is closed by BOTH `content_part.done`
#[test]
fn test_paired_content_and_item_done_emits_single_block_stop() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let _ = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "a"}),
        &mut state,
    );
    assert!(state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));

    let first = reader_read_response_events(
        "response.content_part.done",
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert_eq!(
        first.len(),
        1,
        "content_part.done closes the text block once"
    );
    assert!(!state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));

    let second = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert!(
            second.is_empty(),
            "the second terminal frame for one text item must not emit a duplicate BlockStop, got {second:?}"
        );
}

/// A tool item opened by `output_item.added` is closed by a
/// single `output_item.done`, and a stray second `done` at that index emits nothing.
#[test]
fn test_tool_item_done_emits_single_block_stop() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let _ = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 2,
            "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc_1","name":"f"}
        }),
        &mut state,
    );
    let first = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 2}),
        &mut state,
    );
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 2 }
    ));
    let second = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 2}),
        &mut state,
    );
    assert!(
        second.is_empty(),
        "a closed tool index must not re-emit BlockStop, got {second:?}"
    );
}

/// Every lifecycle event in ONE
/// stream must carry the SAME `response.id`.
#[test]
fn test_terminal_id_matches_created_id_cross_protocol() {
    let writer = ResponsesWriter;

    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("MessageStart emits response.created");
    let created_id = created["response"]["id"]
        .as_str()
        .expect("created carries id")
        .to_string();
    assert!(created_id.starts_with("resp_"));

    let (etype, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("MessageDelta emits terminal");
    assert_eq!(etype, "response.completed");
    assert_eq!(
        completed["response"]["id"].as_str(),
        Some(created_id.as_str()),
        "response.completed.id must equal response.created.id (same stream, same id)"
    );
}

/// A `response.failed` (from an IR Error) must carry
/// the SAME `response.id` as the opening `response.created`, so an SDK correlates the failure
/// with the in-flight Response.
#[test]
fn test_failed_id_matches_created_id_cross_protocol() {
    let writer = ResponsesWriter;
    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("MessageStart emits response.created");
    let created_id = created["response"]["id"].as_str().unwrap().to_string();

    let (etype, failed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("boom".to_string()),
                retry_after: None,
            }),
        )
        .expect("Error emits response.failed");
    assert_eq!(etype, "response.failed");
    assert_eq!(
        failed["response"]["id"].as_str(),
        Some(created_id.as_str()),
        "response.failed.id must equal response.created.id"
    );
}

/// A same-protocol passthrough forwards the upstream
/// `id` on `response.created`, and that SAME id must be replayed on the terminal event.
#[test]
fn test_terminal_id_matches_forwarded_created_id() {
    let writer = ResponsesWriter;
    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: Some("resp_upstream123".to_string()),
                created: Some(42),
                model: None,
            },
        )
        .expect("emit");
    assert_eq!(created["response"]["id"].as_str(), Some("resp_upstream123"));
    let (_, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("emit");
    assert_eq!(
        completed["response"]["id"].as_str(),
        Some("resp_upstream123"),
        "terminal event must replay the forwarded upstream id"
    );
}

/// A stream's id is scoped to ONE writer instance, and a second `MessageStart` on that instance is
/// a DUPLICATE rather than a new stream — the same fact the `started` latch already uses to keep
/// `sequence_number` from rewinding.
#[test]
fn test_carried_id_resets_per_stream() {
    let writer = ResponsesWriter;
    let (_, a_created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: Some("resp_A".to_string()),
                created: None,
                model: None,
            },
        )
        .expect("emit");
    assert_eq!(a_created["response"]["id"].as_str(), Some("resp_A"));

    let (_, dup_created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: Some("resp_B".to_string()),
                created: None,
                model: None,
            },
        )
        .expect("emit");
    assert_eq!(
        dup_created["response"]["id"].as_str(),
        Some("resp_A"),
        "one stream states one id; the duplicate must not re-mint it"
    );
    let (_, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("emit");
    assert_eq!(
        completed["response"]["id"].as_str(),
        Some("resp_A"),
        "the terminal event replays the id the stream opened with"
    );

    let next = ResponsesWriter;
    let (_, b_created) = next
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: Some("resp_B".to_string()),
                created: None,
                model: None,
            },
        )
        .expect("emit");
    assert_eq!(
        b_created["response"]["id"].as_str(),
        Some("resp_B"),
        "a fresh stream's id is its own, not the previous stream's leaked id"
    );
}

/// A backend that emits a `response.output_item.added`
#[test]
fn test_reader_open_tools_is_capped() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    for i in 0..(MAX_OPEN_TOOLS as u64 + 200) {
        let _ = reader_read_response_events(
            EVT_OUTPUT_ITEM_ADDED,
            &serde_json::json!({
                "output_index": i,
                "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc","name":"f"}
            }),
            &mut state,
        );
    }
    assert!(
        state.open_tools.len() <= MAX_OPEN_TOOLS,
        "open_tools must be capped at MAX_OPEN_TOOLS, got {}",
        state.open_tools.len()
    );
}

/// Reject an invalid wire identity instead of aliasing it to a valid boundary block.
#[test]
fn test_reader_output_index_rejected() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let valid = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({"output_index":MAX_OUTPUT_INDEX,
            "item":{"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"valid","name":"f"}}),
        &mut state,
    );
    assert!(
        matches!(valid.as_slice(), [IrStreamEvent::BlockStart { index, .. }] if *index == MAX_OUTPUT_INDEX)
    );
    let out = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({"output_index":u64::MAX,
            "item":{"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"hostile","name":"f"}}),
        &mut state,
    );
    assert!(
        matches!(out.as_slice(), [IrStreamEvent::Error(error)]
        if error.provider_signal.as_deref() == Some("invalid_response_index")),
        "{out:?}"
    );
    assert_eq!(state.open_tools.len(), 1);
    assert!(state.open_tools.contains(&MAX_OUTPUT_INDEX));
}

/// The writer's open-text-index set is also capped so a
/// pathological stream of unique text BlockStarts cannot grow per-stream writer memory without
/// bound.
#[test]
fn test_writer_open_text_indices_capped() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let mut opened = 0usize;
    for i in 0..(MAX_OPEN_TOOLS + 200) {
        if writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: i,
                    block: crate::codec::ir::IrBlockMeta::Text,
                    refusal: false,
                },
            )
            .is_some()
        {
            opened += 1;
        }
    }
    assert!(
        opened <= MAX_OPEN_TOOLS,
        "writer must open at most MAX_OPEN_TOOLS text items, opened {opened}"
    );
}

/// `mark_tool_open` must apply the same cardinality
/// discipline as `open_text_item` — a `contains` guard (idempotent re-mark) plus a
/// `MAX_OPEN_TOOLS` cap — so a pathological backend streaming an unbounded run of distinct
/// function-call indices cannot grow `open_tool_indices` without bound (memory exhaustion).
#[test]
fn test_writer_open_tool_indices_capped() {
    let writer = ResponsesWriter;

    for i in 0..(MAX_OPEN_TOOLS + 200) {
        writer.mark_tool_open(i);
    }

    for i in 0..MAX_OPEN_TOOLS {
        writer.mark_tool_open(i);
    }
    let len = writer.stream().open_tool_indices.len();
    assert!(
        len <= MAX_OPEN_TOOLS,
        "open_tool_indices must be capped at MAX_OPEN_TOOLS, got {len}"
    );
}

/// A function-call open the cap REFUSED must emit no
/// `response.output_item.added`.
#[test]
fn test_writer_refused_tool_open_emits_no_output_item_added() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let mut added = Vec::new();
    for i in 0..(MAX_OPEN_TOOLS + 200) {
        if writer
            .write_response_event(
                crate::codec::test_host::HOST,
                &IrStreamEvent::BlockStart {
                    index: i,
                    block: crate::codec::ir::IrBlockMeta::ToolUse {
                        id: format!("call_{i}"),
                        name: "f".to_string(),
                    },
                    refusal: false,
                },
            )
            .is_some()
        {
            added.push(i);
        }
    }
    assert!(
        added.len() <= MAX_OPEN_TOOLS,
        "the writer must open at most MAX_OPEN_TOOLS function-call items; it emitted \
         {} `output_item.added` frames",
        added.len()
    );

    for i in added {
        assert!(
            writer
                .write_response_event(
                    crate::codec::test_host::HOST,
                    &IrStreamEvent::BlockStop { index: i }
                )
                .is_some(),
            "`output_item.added` at index {i} was never closed by its BlockStop"
        );
    }
}

/// Production `extract_error` must synthesize the
/// canonical `context_length_exceeded` code when an oversized-context error carries the
/// condition only in its MESSAGE (null/generic `code`).
#[test]
fn test_extract_error_synthesizes_context_length_from_message() {
    let reader = ResponsesReader;

    let body = br#"{"error":{"message":"This model's maximum context length is 8192 tokens, however you requested 9000 tokens. Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":null}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded"), // golden wire-contract literal (kept bare on purpose)
        "message-only context-length error must synthesize the canonical code for failover"
    );
    assert_eq!(
        raw.structured_type.as_deref(),
        Some("invalid_request_error")
    );
}

/// A native body that already carries `code: "context_length_exceeded"` must pass through
/// unchanged (the synthesis is `.or_else`, so a real code always wins).
#[test]
fn test_extract_error_preserves_native_context_length_code() {
    let reader = ResponsesReader;
    let body = br#"{"error":{"message":"too long","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let raw = reader.extract_error(StatusCode::BAD_REQUEST, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        Some("context_length_exceeded") // golden wire-contract literal (kept bare on purpose)
    );
}

/// A non-context-length error must NOT be mislabelled as context-length (no false positives
/// from the message scan).
#[test]
fn test_extract_error_unrelated_error_not_context_length() {
    let reader = ResponsesReader;
    let body = br#"{"error":{"message":"Incorrect API key provided.","type":"authentication_error","code":"invalid_api_key"}}"#;
    let raw = reader.extract_error(StatusCode::UNAUTHORIZED, body);
    assert_eq!(raw.provider_code.as_deref(), Some("invalid_api_key"));
}

/// The message-only context-length synthesis is GATED to
/// the oversized HTTP statuses (400/413), mirroring `OpenAiReader::extract_error`.
#[test]
fn test_extract_error_oversized_phrase_on_non_oversized_status_not_synthesized() {
    let reader = ResponsesReader;

    let body = br#"{"error":{"message":"This model's maximum context length is 8192 tokens.","type":"rate_limit_error","code":null}}"#;
    let raw = reader.extract_error(StatusCode::TOO_MANY_REQUESTS, body);
    assert_eq!(
        raw.provider_code.as_deref(),
        None,
        "a 429 mentioning context length must NOT be reclassified as context_length_exceeded"
    );

    let raw_401 = reader.extract_error(StatusCode::UNAUTHORIZED, body);
    assert_eq!(
        raw_401.provider_code.as_deref(),
        None,
        "a 401 mentioning context length must NOT be reclassified as context_length_exceeded"
    );
}

/// `ResponsesReader::classify` delegates to `openai_chat::openai_classify`
#[test]
fn test_responses_classify_delegates() {
    let reader = ResponsesReader;
    let signal = reader.classify(StatusCode::TOO_MANY_REQUESTS, b"{}");
    assert_eq!(signal.class, StatusClass::RateLimit);

    let ctx_body =
        br#"{"error":{"message":"This model's maximum context length is 8192 tokens."}}"#;
    let signal = reader.classify(StatusCode::TOO_MANY_REQUESTS, ctx_body);
    assert_eq!(
        signal.class,
        StatusClass::RateLimit,
        "a 429 mentioning context length must stay RateLimit, not ContextLength"
    );
}

/// A max_tokens-truncated stream's terminal event must
/// be `response.incomplete` (event name AND inner `type`), NOT `response.completed`.
#[test]
fn test_terminal_incomplete_emits_response_incomplete_for_max_tokens() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (etype, body) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::MaxTokens),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("MessageDelta emits a terminal event");
    assert_eq!(
        etype,
        "response.incomplete", // golden wire-contract literal (kept bare on purpose)
        "max_tokens truncation must use the response.incomplete event name"
    );
    assert_eq!(
        body["type"].as_str(),
        Some("response.incomplete"), // golden wire-contract literal (kept bare on purpose)
        "inner dispatch type must agree with the event name"
    );
    assert_eq!(
        body["response"]["status"].as_str(),
        Some("incomplete"), // golden wire-contract literal (kept bare on purpose)
        "inner status stays incomplete"
    );
    assert_eq!(
        body["response"]["incomplete_details"]["reason"].as_str(),
        Some("max_output_tokens"), // golden wire-contract literal (kept bare on purpose)
        "incomplete_details.reason maps max_tokens → max_output_tokens"
    );
}

/// A safety/content-filter stop is also `incomplete`,
/// so its terminal event is `response.incomplete` with reason `content_filter`.
#[test]
fn test_terminal_incomplete_emits_response_incomplete_for_safety() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (etype, body) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::Safety),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("MessageDelta emits a terminal event");
    assert_eq!(etype, "response.incomplete");
    assert_eq!(body["type"].as_str(), Some("response.incomplete"));
    assert_eq!(
        body["response"]["incomplete_details"]["reason"].as_str(),
        Some("content_filter") // golden wire-contract literal (kept bare on purpose)
    );
}

/// A normally-completed stream still emits
/// `response.completed` with inner type/status `completed` — the fix must not regress the
/// success path.
#[test]
fn test_terminal_completed_unchanged_for_end_turn() {
    let writer = ResponsesWriter;
    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("created");
    let created_id = created["response"]["id"].as_str().unwrap().to_string();
    let (etype, body) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal");
    assert_eq!(etype, "response.completed");
    assert_eq!(body["type"].as_str(), Some("response.completed"));
    assert_eq!(body["response"]["status"].as_str(), Some("completed"));

    assert_eq!(
        body["response"].get("incomplete_details"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(body["response"]["id"].as_str(), Some(created_id.as_str()));
}

/// Write_error must NOT leak the Anthropic-vocabulary
/// `overloaded` type to an OpenAI-family client.
#[test]
fn test_write_error_maps_overloaded_to_server_error() {
    let writer = ResponsesWriter;
    for kind in [
        "overloaded",
        ERR_TYPE_OVERLOADED,
        "service_unavailable",
        "unavailable",
    ] {
        let v = writer.write_error(crate::codec::test_host::HOST, 503, kind, "upstream busy");
        assert_eq!(
            v["error"]["type"].as_str(),
            Some("server_error"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind:?} must map to server_error, never leak overloaded"
        );

        assert!(v["error"]["code"].is_null(), "server_error code is null");
    }
}

/// Synthesized `resp_` ids must be opaque base62 of
/// native length with NO embedded timestamp or sequential structure, so an observer cannot
/// fingerprint a proxied response or extract the server clock from the id.
#[test]
fn test_synthesize_response_id_is_opaque_native_length() {
    let id = synthesize_response_id(crate::codec::test_host::HOST).expect("test entropy");
    let suffix = id.strip_prefix("resp_").expect("resp_ prefix");
    assert_eq!(
        suffix.len(),
        RESPONSE_ID_TOKEN_LEN,
        "native-length suffix: {id}"
    );
    assert!(
        suffix.len() >= 38,
        "at least the native ~38-char profile: {id}"
    );
    assert!(
        suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
        "opaque base62 suffix only: {id}"
    );

    assert_eq!(
        id.matches('_').count(),
        1,
        "no internal field delimiter: {id}"
    );
}

/// Synthesized `msg_`/`fc_` item ids must be opaque
/// base62 of native length, NOT the old sequential `msg_00000000` positional hex.
#[test]
fn test_synthesize_item_id_is_opaque_native_length() {
    for prefix in ["msg", "fc"] {
        let id = synthesize_item_id(crate::codec::test_host::HOST, prefix).expect("test entropy");
        let suffix = id
            .strip_prefix(&format!("{prefix}_"))
            .expect("prefix present");
        assert_eq!(
            suffix.len(),
            ITEM_ID_TOKEN_LEN,
            "native-length suffix: {id}"
        );
        assert!(
            suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
            "opaque base62 suffix: {id}"
        );

        assert_ne!(
            suffix,
            "0".repeat(ITEM_ID_TOKEN_LEN),
            "not all-zero positional: {id}"
        );
    }
}

/// Synthesized ids must be unique across calls even in a tight loop (the monotonic counter folded
/// into the token guarantees this independent of the RNG).
#[test]
fn test_synthesized_ids_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        assert!(
            seen.insert(
                synthesize_response_id(crate::codec::test_host::HOST).expect("test entropy")
            ),
            "duplicate resp_ id"
        );
        assert!(
            seen.insert(
                synthesize_item_id(crate::codec::test_host::HOST, "msg").expect("test entropy")
            ),
            "duplicate msg_ id"
        );
    }
}

/// The writer's `item_id_for` cache must return the SAME opaque id for a `(prefix, index)` across
/// the item's lifecycle (so `output_item.added`/delta/`output_item.done` correlate), distinct ids
/// for different indices, and a fresh id for a new stream after `reset_sequence_number`.
#[test]
fn test_item_id_for_is_stream_stable_and_opaque() {
    let writer = ResponsesWriter;
    let a1 = writer
        .item_id_for(crate::codec::test_host::HOST, "msg", 0)
        .expect("the controlled host supplies item-id entropy");
    let a2 = writer
        .item_id_for(crate::codec::test_host::HOST, "msg", 0)
        .expect("the cached item id remains available");
    assert_eq!(
        a1, a2,
        "same (prefix,index) yields a stable id within a stream"
    );
    assert!(a1.starts_with("msg_"));
    let b = writer
        .item_id_for(crate::codec::test_host::HOST, "msg", 1)
        .expect("the controlled host supplies another item's entropy");
    assert_ne!(a1, b, "different indices get distinct ids");
    let fc = writer
        .item_id_for(crate::codec::test_host::HOST, "fc", 0)
        .expect("the controlled host supplies function-item entropy");
    assert_ne!(
        a1, fc,
        "different prefixes at the same index get distinct ids"
    );
    assert!(fc.starts_with("fc_"));

    writer.reset_sequence_number();
    let a_after = writer
        .item_id_for(crate::codec::test_host::HOST, "msg", 0)
        .expect("the controlled host supplies the next stream's entropy");
    assert_ne!(
        a1, a_after,
        "a reused writer must not replay a previous stream's item id"
    );
}

/// Full streamed text item: the `output_item.added`, the `content_part.added`, every
/// `output_text.delta`, and the closing `output_text.done` / `content_part.done` /
/// `output_item.done` must ALL carry the SAME `item_id` so a typed SDK correlates the lifecycle.
#[test]
fn test_streamed_text_item_shares_one_item_id() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let added_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );

    let added_id = added_frames[0].1["item_id"].as_str().unwrap().to_string();
    assert!(added_id.starts_with("msg_"));
    assert_eq!(added_frames[1].0, "response.content_part.added");
    assert_eq!(
        added_frames[1].1["item_id"].as_str(),
        Some(added_id.as_str())
    );

    let (_, delta) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::TextDelta("hello".to_string()),
            },
        )
        .expect("output_text.delta");
    assert_eq!(delta["item_id"].as_str(), Some(added_id.as_str()));

    let done_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );

    assert_eq!(done_frames.len(), 3, "text close is a three-frame bracket");
    for (et, frame) in &done_frames {
        assert_eq!(
            frame["item_id"].as_str(),
            Some(added_id.as_str()),
            "every close frame ({et}) must share the added item_id"
        );
    }
    let done = &done_frames[2].1;
    assert_eq!(done["item"]["id"].as_str(), Some(added_id.as_str()));
}

/// The cardinality-cap guard on
/// `response.output_item.added` was inverted (`if already_open || ...`), re-emitting a BlockStart
/// for an index that was already open.
#[test]
fn test_repeated_output_item_added_does_not_reemit_block_start() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let item = serde_json::json!({
        "output_index": 0,
        "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc_1","name":"f"}
    });
    let first = reader_read_response_events(EVT_OUTPUT_ITEM_ADDED, &item, &mut state);
    assert_eq!(
        first.len(),
        1,
        "the first output_item.added opens exactly one BlockStart"
    );
    assert!(matches!(
        first.first(),
        Some(crate::codec::ir::IrStreamEvent::BlockStart { index: 0, .. })
    ));

    let second = reader_read_response_events(EVT_OUTPUT_ITEM_ADDED, &item, &mut state);
    assert!(
            second.is_empty(),
            "a repeated output_item.added for an open index must not re-emit BlockStart, got {second:?}"
        );
    assert_eq!(
        state.open_tools.len(),
        1,
        "the index is tracked exactly once"
    );
}

/// The fixed guard must STILL bound new distinct
/// indices under MAX_OPEN_TOOLS — the inversion fix must not weaken the DoS cap.
#[test]
fn test_cap_still_bounds_new_indices_after_guard_fix() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    for i in 0..(MAX_OPEN_TOOLS as u64) {
        let out = reader_read_response_events(
            EVT_OUTPUT_ITEM_ADDED,
            &serde_json::json!({
                "output_index": i,
                "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc","name":"f"}
            }),
            &mut state,
        );
        assert_eq!(out.len(), 1, "each fresh in-cap index opens one BlockStart");
    }
    assert_eq!(state.open_tools.len(), MAX_OPEN_TOOLS);

    let over = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": (MAX_OPEN_TOOLS as u64) + 50,
            "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc","name":"f"}
        }),
        &mut state,
    );

    assert!(
        matches!(over.as_slice(), [IrStreamEvent::Error(error)]
            if error.provider_signal.as_deref() == Some("invalid_response_index")),
        "an over-cap index must explicitly refuse without aliasing an open block, got {over:?}"
    );
    assert!(
        state.open_tools.len() <= MAX_OPEN_TOOLS,
        "open_tools must never exceed MAX_OPEN_TOOLS, got {}",
        state.open_tools.len()
    );
}

/// A `function_call_arguments.delta` for an index
#[test]
fn test_args_delta_dropped_for_unopened_index() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let out = reader_read_response_events(
        EVT_FUNCTION_CALL_ARGS_DELTA,
        &serde_json::json!({"output_index": 3, "delta": "{\"a\":1}"}),
        &mut state,
    );
    assert!(
        out.is_empty(),
        "args delta for an unopened index must be dropped, got {out:?}"
    );
}

/// A `function_call_arguments.delta` for an index
#[test]
fn test_args_delta_routed_for_opened_index() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let _ = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 1,
            "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc","name":"f"}
        }),
        &mut state,
    );
    let out = reader_read_response_events(
        EVT_FUNCTION_CALL_ARGS_DELTA,
        &serde_json::json!({"output_index": 1, "delta": "{\"a\":1}"}),
        &mut state,
    );
    match out.first() {
        Some(crate::codec::ir::IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::InputJsonDelta(s),
        }) => {
            assert_eq!(*index, 1);
            assert_eq!(s, "{\"a\":1}");
        }
        other => panic!("expected InputJsonDelta at index 1, got {other:?}"),
    }
}

/// Every lifecycle event in a stream must carry the
/// SAME `created_at` as the opening `response.created` — the terminal event must replay the
/// captured timestamp, not a fresh `now_unix_secs()` wall-clock read.
#[test]
fn test_created_at_is_constant_across_stream_events() {
    let writer = ResponsesWriter;
    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: Some(1_700_000_000),
                model: None,
            },
        )
        .expect("response.created");
    let created_ts = created["response"]["created_at"].as_u64();
    assert_eq!(created_ts, Some(1_700_000_000));

    let (_, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal event");
    assert_eq!(
        completed["response"]["created_at"].as_u64(),
        created_ts,
        "terminal created_at must match the opening event's"
    );
}

/// The `response.failed` event must also replay the
/// captured `created_at`, matching `response.created`.
#[test]
fn test_failed_event_replays_created_at() {
    let writer = ResponsesWriter;
    let (_, created) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: Some(1_700_000_123),
                model: None,
            },
        )
        .expect("response.created");
    let created_ts = created["response"]["created_at"].as_u64();

    let (_, failed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("boom".to_string()),
                retry_after: None,
            }),
        )
        .expect("response.failed");
    assert_eq!(
        failed["response"]["created_at"].as_u64(),
        created_ts,
        "response.failed created_at must match response.created"
    );
}

/// The proxy engine transient/upstream error kinds
/// (`timeout`/`network`/`connect`/`5xx`/`transient`/`api_error`) must map to the native
/// `server_error` type, and `context_length_exceeded`/`bad_request` to `invalid_request_error`,
/// never leaking a non-native `error.type` to a Responses client.
#[test]
fn test_write_error_maps_forward_transient_kinds() {
    let writer = ResponsesWriter;
    for kind in [
        "timeout",
        "network",
        "connect",
        "5xx",
        "transient",
        "api_error",
    ] {
        let v = writer.write_error(crate::codec::test_host::HOST, 503, kind, "upstream failure");
        assert_eq!(
            v["error"]["type"].as_str(),
            Some("server_error"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind:?} must map to server_error"
        );
        assert!(v["error"]["code"].is_null(), "server_error code is null");
    }
    for kind in [
        crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH,
        "bad_request",
    ] {
        let v = writer.write_error(crate::codec::test_host::HOST, 400, kind, "bad request");
        assert_eq!(
            v["error"]["type"].as_str(),
            Some("invalid_request_error"), // golden wire-contract literal (kept bare on purpose)
            "kind {kind:?} must map to invalid_request_error"
        );
    }
}

/// The function-call `response.output_item.done`
#[test]
fn test_function_call_done_carries_finalized_item() {
    let writer = ResponsesWriter;

    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );

    let added = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::ToolUse {
                    id: "call_abc".to_string(),
                    name: "get_weather".to_string(),
                },
                refusal: false,
            },
        )
        .expect("output_item.added should emit");
    assert_eq!(added.0, "response.output_item.added");

    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("{\"city\":".to_string()),
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("\"SF\"}".to_string()),
        },
    );

    let frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    );
    assert_eq!(
        frames
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "response.function_call_arguments.done",
            "response.output_item.done"
        ]
    );
    let item = &frames[1].1["item"];
    assert_eq!(frames[0].1["arguments"], item["arguments"]);
    assert_eq!(frames[0].1["item_id"], item["id"]);
    assert_eq!(item["type"].as_str(), Some("function_call"));
    assert_eq!(
        item["call_id"].as_str(),
        Some("call_abc"),
        "done item must carry call_id"
    );
    assert_eq!(
        item["name"].as_str(),
        Some("get_weather"),
        "done item must carry name"
    );
    assert_eq!(
        item["arguments"].as_str(),
        Some("{\"city\":\"SF\"}"),
        "done item must carry the COMPLETE accumulated arguments"
    );

    assert_eq!(item["id"], added.1["item"]["id"]);
}

/// The streaming READER must track open text blocks
/// PER `output_index`, not with a single index-blind bool.
#[test]
fn test_reader_multiple_text_items_distinct_indices() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let a = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "alpha"}),
        &mut state,
    );
    assert_eq!(a.len(), 2, "first delta opens its block then writes");
    assert!(matches!(
        a[0],
        crate::codec::ir::IrStreamEvent::BlockStart { index: 0, .. }
    ));

    let b = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 1, "delta": "beta"}),
        &mut state,
    );
    assert_eq!(b.len(), 2, "a new index must lazily open its own block");
    assert!(
        matches!(
            b[0],
            crate::codec::ir::IrStreamEvent::BlockStart { index: 1, .. }
        ),
        "second text index must emit its OWN BlockStart, got {:?}",
        b[0]
    );
    assert!(matches!(
        b[1],
        crate::codec::ir::IrStreamEvent::BlockDelta { index: 1, .. }
    ));

    let close0 = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert_eq!(close0.len(), 1);
    assert!(
        matches!(
            close0[0],
            crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
        ),
        "close must pair with index 0, got {:?}",
        close0[0]
    );

    let close1 = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 1}),
        &mut state,
    );
    assert_eq!(close1.len(), 1);
    assert!(matches!(
        close1[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 1 }
    ));
}

/// A tool item and a text item at DISTINCT indices
/// in the same stream must not interfere — the tool index routes its arguments delta and closes
/// as a tool, while the text index opens/closes independently.
#[test]
fn test_reader_text_and_tool_indices_coexist() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let _ = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 0,
            "item": {"type":ITEM_TYPE_FUNCTION_CALL,"call_id":"fc_1","name":"f"}
        }),
        &mut state,
    );

    let t = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 1, "delta": "hi"}),
        &mut state,
    );
    assert!(matches!(
        t[0],
        crate::codec::ir::IrStreamEvent::BlockStart { index: 1, .. }
    ));

    let args = reader_read_response_events(
        EVT_FUNCTION_CALL_ARGS_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "{\"x\":1}"}),
        &mut state,
    );
    assert_eq!(args.len(), 1, "tool args delta must route to the open tool");
    assert!(matches!(
        args[0],
        crate::codec::ir::IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::InputJsonDelta(_)
        }
    ));

    let close_tool = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 0}),
        &mut state,
    );
    assert!(matches!(
        close_tool[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }
    ));

    let close_text = reader_read_response_events(
        EVT_OUTPUT_ITEM_DONE,
        &serde_json::json!({"output_index": 1}),
        &mut state,
    );
    assert!(matches!(
        close_text[0],
        crate::codec::ir::IrStreamEvent::BlockStop { index: 1 }
    ));
}

/// `write_response` must emit the SDK-required non-nullable
/// `model` even when the IR carries none (cross-protocol path, e.g. Bedrock/Anthropic →
/// Responses). A prior revision emitted `model` only when `resp.model` was `Some`, dropping the
/// key entirely on cross-protocol responses — a strict-decoder failure and a distinguishability
/// tell. Absent IR model must fall back to DEFAULT_MODEL; a present model is preserved verbatim.
#[test]
fn test_write_response_emits_model_fallback() {
    let make_resp = |model: Option<String>| crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model,
        id: Some("resp_x".to_string()),
        created: Some(1),
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let writer = ResponsesWriter;
    let out_none = writer.write_response(crate::codec::test_host::HOST, &make_resp(None));
    assert_eq!(
        out_none.get("model").and_then(|m| m.as_str()),
        Some(DEFAULT_MODEL),
        "absent model must fall back to DEFAULT_MODEL, not be omitted"
    );

    let writer_some = ResponsesWriter;
    let out_some = writer_some.write_response(
        crate::codec::test_host::HOST,
        &make_resp(Some("gpt-4o-mini".to_string())),
    );
    assert_eq!(
        out_some.get("model").and_then(|m| m.as_str()),
        Some("gpt-4o-mini"),
        "present model must be preserved verbatim"
    );
}

/// On a cross-protocol stream (IR `model` is `None` on
/// `MessageStart`), `response.created` AND every terminal lifecycle event
/// (`response.completed`/`.incomplete`/`.failed`) must carry the same non-nullable `model`
/// (DEFAULT_MODEL here).
#[test]
fn test_stream_terminal_events_carry_model_fallback() {
    let writer = ResponsesWriter;
    let start = crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: None,
        created: None,
        model: None,
    };
    let (_, created) = writer
        .write_response_event(crate::codec::test_host::HOST, &start)
        .expect("created event");
    assert_eq!(
        created
            .get("response")
            .and_then(|r| r.get("model"))
            .and_then(|m| m.as_str()),
        Some(DEFAULT_MODEL),
        "response.created must carry DEFAULT_MODEL when IR model is None"
    );

    let delta = crate::codec::ir::IrStreamEvent::MessageDelta {
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        stop_detail: None,
    };
    let (ename, completed) = writer
        .write_response_event(crate::codec::test_host::HOST, &delta)
        .expect("terminal event");
    assert_eq!(ename, "response.completed");
    assert_eq!(
        completed
            .get("response")
            .and_then(|r| r.get("model"))
            .and_then(|m| m.as_str()),
        Some(DEFAULT_MODEL),
        "response.completed must replay the required model field"
    );

    let writer2 = ResponsesWriter;
    let start2 = crate::codec::ir::IrStreamEvent::MessageStart {
        role: crate::codec::ir::IrRole::Assistant,
        usage: None,
        id: Some("resp_keep".to_string()),
        created: Some(1_720_000_000),
        model: Some("gpt-4o-mini".to_string()),
    };
    writer2
        .write_response_event(crate::codec::test_host::HOST, &start2)
        .expect("created event");
    let err = crate::codec::ir::IrStreamEvent::Error(IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("boom".to_string()),
        retry_after: None,
    });
    let (ename2, failed) = writer2
        .write_response_event(crate::codec::test_host::HOST, &err)
        .expect("failed event");
    assert_eq!(ename2, "response.failed");
    assert_eq!(
        failed
            .get("response")
            .and_then(|r| r.get("model"))
            .and_then(|m| m.as_str()),
        Some("gpt-4o-mini"),
        "response.failed must replay the captured stream model"
    );
}

/// The non-streaming `write_response` body must carry
/// the REQUIRED nullable `error` field (`null` on a non-failed response), mirroring the
/// streaming `response.created` skeleton.
#[test]
fn test_write_response_emits_error_null_for_completed_and_incomplete() {
    let make_resp = |stop: crate::codec::ir::IrStopReason| crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(stop),
        usage: usage_fixture(),
        model: Some("gpt-4o-mini".to_string()),
        id: Some("resp_x".to_string()),
        created: Some(1),
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let writer = ResponsesWriter;

    let completed = writer.write_response(
        crate::codec::test_host::HOST,
        &make_resp(crate::codec::ir::IrStopReason::EndTurn),
    );
    assert_eq!(completed["status"].as_str(), Some("completed"));
    assert!(
        completed.get("error").is_some(),
        "non-streaming body must include the required error key"
    );
    assert!(
        completed["error"].is_null(),
        "error must be null on a completed response"
    );

    let incomplete = writer.write_response(
        crate::codec::test_host::HOST,
        &make_resp(crate::codec::ir::IrStopReason::MaxTokens),
    );
    assert_eq!(incomplete["status"].as_str(), Some("incomplete"));
    assert!(
        incomplete["error"].is_null(),
        "error must be null on an incomplete response"
    );
}

/// A non-streaming
/// Responses body with `status:"failed"` and `output:null` is an upstream provider failure, NOT
/// a parse error.
#[test]
fn test_read_response_failed_surfaces_upstream_error() {
    let reader = ResponsesReader;

    let body = serde_json::json!({
        "id": "resp_fail",
        "object": OBJ_RESPONSE,
        "status": STATUS_FAILED,
        "output": serde_json::Value::Null,
        "error": { "code": ERR_CODE_RATE_LIMIT, "message": "slow down" },
        "usage": { "input_tokens": 1, "output_tokens": 0 },
        "model": "gpt-4o-mini"
    });
    let err = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect_err("failed status must surface as an error");
    assert_eq!(
        err.class,
        StatusClass::RateLimit,
        "a rate_limit_exceeded failed body is a RateLimit, not a generic ServerError"
    );
    assert_eq!(
        err.provider_signal.as_deref(),
        Some("rate_limit_exceeded"),
        "the upstream error.code must be surfaced as the provider signal"
    );

    let body_type = serde_json::json!({
        "status": STATUS_FAILED,
        "error": { "type": INCOMPLETE_REASON_CONTENT_FILTER, "message": "blocked" },
        "usage": { "input_tokens": 1, "output_tokens": 0 }
    });
    let err_type = reader
        .read_response(crate::codec::test_host::HOST, &body_type)
        .expect_err("failed status must surface as an error");
    assert_eq!(err_type.class, StatusClass::ServerError);
    assert_eq!(err_type.provider_signal.as_deref(), Some("content_filter"));

    let body_bare = serde_json::json!({ "status": STATUS_FAILED });
    let err_bare = reader
        .read_response(crate::codec::test_host::HOST, &body_bare)
        .expect_err("failed status must surface as an error");
    assert_eq!(err_bare.class, StatusClass::ServerError);
    assert_eq!(err_bare.provider_signal.as_deref(), Some("response_failed"));

    let body_parse = serde_json::json!({ "id": "resp_x" });
    let err_parse = reader
        .read_response(crate::codec::test_host::HOST, &body_parse)
        .expect_err("missing output must surface as a parse error");
    assert_eq!(err_parse.class, StatusClass::ClientError);
    assert_eq!(err_parse.provider_signal.as_deref(), Some("ir_parse"));
}

/// The writer emits a failed body as
/// `{"status":"failed","output":[],"error":{...}}` — `output` is a PRESENT EMPTY array, not
/// null/absent.
#[test]
fn test_read_response_failed_with_empty_output_array_not_masked() {
    let reader = ResponsesReader;
    let body = serde_json::json!({
        "id": "resp_fail",
        "object": OBJ_RESPONSE,
        "status": STATUS_FAILED,
        "output": [],
        "error": { "code": ERR_TYPE_SERVER_ERROR, "message": "boom" },
        // No `usage` on purpose: pre-fix this body fell through to the usage check and that
        // was part of what masked the error. The failed early return must not require usage.
    });
    let err = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect_err("failed status with output:[] must surface as an error, not be masked");
    assert_eq!(
        err.class,
        StatusClass::ServerError,
        "output:[] on a failed body must still be a ServerError, not a ClientError ir_parse"
    );
    assert_eq!(
        err.provider_signal.as_deref(),
        Some("server_error"),
        "the upstream error.code must be carried through even with output:[]"
    );
}

/// `system`/`developer` input turns carry the system
/// prompt.
#[test]
fn test_read_request_system_and_developer_turns_feed_system() {
    let reader = ResponsesReader;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            // typed message, system role, array content
            { "type": ITEM_TYPE_MESSAGE, "role": "system",
              "content": [{ "type": CONTENT_TYPE_INPUT_TEXT, "text": "you are terse" }] },
            // typed message, developer role, bare-string content
            { "type": ITEM_TYPE_MESSAGE, "role": "developer", "content": "be precise" },
            // untyped item, system role, array content
            { "role": "system",
              "content": [{ "type": CONTENT_TYPE_INPUT_TEXT, "text": "no emojis" }] },
            // a normal user turn must still land in messages
            { "type": CONTENT_TYPE_INPUT_TEXT, "text": "hello" }
        ]
    });
    let req = reader.read_request(&body).expect("request must parse");
    let system_text: Vec<&str> = req
        .system
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        system_text,
        vec!["you are terse", "be precise", "no emojis"],
        "system/developer turns (typed+untyped, array+string) must feed IrRequest.system in order"
    );

    assert_eq!(
        req.messages.len(),
        1,
        "only the user turn becomes a message"
    );
    assert!(
        matches!(req.messages[0].role, crate::codec::ir::IrRole::User),
        "the surviving message is the user turn"
    );
}

/// `max_output_tokens` was read via
/// `.as_i64()...map(|v| v as u32)`, silently truncating a value larger than `u32::MAX`.
#[test]
fn test_read_request_max_output_tokens_out_of_range_drops_to_none() {
    let reader = ResponsesReader;

    let big = u64::from(u32::MAX) + 1;
    let body_big = serde_json::json!({
        "model": "gpt-4o",
        "input": "hi",
        "max_output_tokens": big
    });
    let req_big = reader.read_request(&body_big).expect("request must parse");
    assert_eq!(
        req_big.max_tokens, None,
        "an out-of-range max_output_tokens must drop to None, not truncate"
    );

    let body_ok = serde_json::json!({
        "model": "gpt-4o",
        "input": "hi",
        "max_output_tokens": 4096
    });
    let req_ok = reader.read_request(&body_ok).expect("request must parse");
    assert_eq!(req_ok.max_tokens, Some(4096));
}

/// The terminal `response.completed`/
/// `response.incomplete`/`response.failed` events' inner `response` object must carry the
/// REQUIRED `output` array (present-but-empty) and (on non-failed terminals) `error: null`,
/// mirroring the `response.created` skeleton.
#[test]
fn test_stream_terminal_events_carry_output_and_error() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (_, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal event");
    assert!(
        completed["response"]["output"].is_array(),
        "response.completed inner response must carry an output array"
    );
    assert!(
        completed["response"]["error"].is_null(),
        "response.completed inner response must carry error: null"
    );

    let writer2 = ResponsesWriter;
    let _ = writer2.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (_, incomplete) = writer2
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::MaxTokens),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal event");
    assert!(
        incomplete["response"]["output"].is_array(),
        "response.incomplete inner response must carry an output array"
    );
    assert!(
        incomplete["response"]["error"].is_null(),
        "response.incomplete inner response must carry error: null"
    );

    let writer3 = ResponsesWriter;
    let _ = writer3.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (ename, failed) = writer3
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("boom".to_string()),
                retry_after: None,
            }),
        )
        .expect("failed event");
    assert_eq!(ename, "response.failed");
    assert!(
        failed["response"]["output"].is_array(),
        "response.failed inner response must carry an output array"
    );
    assert_eq!(
        failed["response"]["error"]["code"].as_str(),
        Some("server_error"),
        "response.failed must still carry its populated error object"
    );
}

/// The terminal `response.completed` event's inner
#[test]
fn test_terminal_output_assembles_streamed_text_and_tool_items() {
    let writer = ResponsesWriter;

    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );

    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("Hello ".to_string()),
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("world".to_string()),
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );

    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 1,
            block: crate::codec::ir::IrBlockMeta::ToolUse {
                id: "call_abc".to_string(),
                name: "get_weather".to_string(),
            },
            refusal: false,
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 1,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("{\"city\":".to_string()),
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 1,
            delta: crate::codec::ir::IrDelta::InputJsonDelta("\"SF\"}".to_string()),
        },
    );
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 1 },
    );

    let (ename, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::ToolUse),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal event");
    assert_eq!(ename, "response.completed");

    let output = completed["response"]["output"]
        .as_array()
        .expect("terminal output must be an array");
    assert_eq!(
        output.len(),
        2,
        "assembled output must carry both the message and the function_call item, got {output:?}"
    );

    let msg_item = &output[0];
    assert_eq!(msg_item["type"], serde_json::json!("message"));
    assert_eq!(msg_item["role"], serde_json::json!("assistant"));
    let text = msg_item["content"][0]["text"]
        .as_str()
        .expect("message item carries assembled output_text");
    assert_eq!(
        text, "Hello world",
        "the streamed text must be fully assembled"
    );

    let fc_item = &output[1];
    assert_eq!(fc_item["type"], serde_json::json!("function_call"));
    assert_eq!(fc_item["call_id"], serde_json::json!("call_abc"));
    assert_eq!(fc_item["name"], serde_json::json!("get_weather"));
    assert_eq!(
        fc_item["arguments"],
        serde_json::json!("{\"city\":\"SF\"}"),
        "the finalized function-call item must carry the complete accumulated arguments"
    );
}

/// A genuinely output-less turn (no blocks streamed) still emits a present-but-empty `output`
#[test]
fn test_terminal_output_empty_when_no_blocks_streamed() {
    let writer = ResponsesWriter;
    let _ = writer.write_response_event(
        crate::codec::test_host::HOST,
        &IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
    );
    let (_, completed) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("terminal event");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output present-but-empty, never omitted");
    assert!(
        output.is_empty(),
        "no blocks streamed -> empty output array"
    );
}

/// A function_call and a text part arriving at the SAME `output_index`
#[test]
fn test_same_output_index_tool_and_text_single_open_single_close() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let added = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 0,
            "item": {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "call_x", "name": "f"}
        }),
        &mut state,
    );
    let starts_after_tool = added
        .iter()
        .filter(|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStart { .. }))
        .count();
    assert_eq!(
        starts_after_tool, 1,
        "tool open emits exactly one BlockStart"
    );

    let text = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "hi"}),
        &mut state,
    );
    assert!(
        text.is_empty(),
        "text delta at an index already held by a tool block must emit nothing, got {text:?}"
    );
    assert!(
        !state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET),
        "no text key must be opened at an index already held by a tool"
    );

    assert_eq!(
        state.open_tools.len(),
        1,
        "exactly one open marker for the shared index"
    );

    let completed = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({"response": {"status": STATUS_COMPLETED}}),
        &mut state,
    );
    let stops_at_0 = completed
        .iter()
        .filter(|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStop { index: 0 }))
        .count();
    assert_eq!(
        stops_at_0, 1,
        "terminal must emit EXACTLY ONE BlockStop for the shared index, got {completed:?}"
    );
}

/// Even if two open keys for the same IR index somehow
/// coexist in `open_tools` (raw `N` and `N + TEXT_INDEX_KEY_OFFSET`), the terminal drain must
/// collapse them to a SINGLE `BlockStop{N}`.
#[test]
fn test_terminal_drain_dedups_colliding_keys() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    state.open_tools.insert(3);
    state.open_tools.insert(3 + TEXT_INDEX_KEY_OFFSET);

    let events = reader_read_response_events(
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({"response": {"status": STATUS_COMPLETED}}),
        &mut state,
    );
    let stops_at_3 = events
        .iter()
        .filter(|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStop { index: 3 }))
        .count();
    assert_eq!(
            stops_at_3, 1,
            "colliding tool+text keys for one IR index must drain to exactly one BlockStop, got {events:?}"
        );
    assert!(
        state.open_tools.is_empty(),
        "terminal drain must clear all open keys"
    );
}

/// A tool item must not open at an `output_index` already
/// held by an OPEN TEXT block.
#[test]
fn test_tool_open_suppressed_when_text_block_open_at_index() {
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let text = reader_read_response_events(
        EVT_OUTPUT_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "a"}),
        &mut state,
    );
    assert!(state.open_tools.contains(&TEXT_INDEX_KEY_OFFSET));
    assert_eq!(text.len(), 2);

    let added = reader_read_response_events(
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 0,
            "item": {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "call_y", "name": "g"}
        }),
        &mut state,
    );
    assert!(
        !added
            .iter()
            .any(|e| matches!(e, crate::codec::ir::IrStreamEvent::BlockStart { .. })),
        "tool open at an index already held by a text block must emit no BlockStart, got {added:?}"
    );
    assert!(
        !state.open_tools.contains(&0),
        "no raw tool key may be opened at an index already held by a text block"
    );
}

/// `synth_token` must emit ONLY base62 characters, drawn uniformly via
/// rejection sampling (no biased `byte % 62`).
#[test]
fn test_synth_token_uniform_base62_only() {
    let alphabet: std::collections::HashSet<char> = BASE62.iter().map(|&b| b as char).collect();

    let mut counts = [0usize; 62];
    let char_to_index: std::collections::HashMap<char, usize> = BASE62
        .iter()
        .enumerate()
        .map(|(i, &b)| (b as char, i))
        .collect();

    for _ in 0..20_000 {
        let tok = synth_token::<48>(crate::codec::test_host::HOST).expect("test entropy");
        assert_eq!(tok.len(), 48, "token width must be exactly N");
        for c in tok.chars() {
            assert!(
                alphabet.contains(&c),
                "synth_token produced a non-base62 char: {c:?}"
            );
            counts[char_to_index[&c]] += 1;
        }
    }

    assert!(
        counts.iter().all(|&n| n > 0),
        "all 62 base62 digits should appear in a large uniform sample, counts={counts:?}"
    );

    let biased_group: f64 = counts[0..8].iter().sum::<usize>() as f64 / 8.0;
    let rest_group: f64 = counts[8..62].iter().sum::<usize>() as f64 / 54.0;
    let ratio = biased_group / rest_group;

    assert!(
        (0.95..=1.05).contains(&ratio),
        "first-8 base62 digits must not be over-represented (group ratio={ratio:.4}; \
             ~1.25 indicates the biased `byte % 62` reduction). counts={counts:?}"
    );
}

#[test]
fn test_responses_tool_choice_required_roundtrips() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "name": "get_weather", "parameters": {}}],
        "tool_choice": "required",
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&json).expect("read_request");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Required)
    );

    assert!(!ir.extra.contains_key("tool_choice"));

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out.get("tool_choice").and_then(|v| v.as_str()),
        Some("required")
    );
}

#[test]
fn test_responses_tool_choice_specific_function() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "function", "name": "get_weather", "parameters": {}}],
        "tool_choice": {"type": "function", "name": "get_weather"},
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&json).expect("read_request");
    assert_eq!(
        ir.tool_choice,
        Some(crate::codec::ir::IrToolChoice::Tool {
            name: "get_weather".to_string()
        })
    );
    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let tc = out.get("tool_choice").expect("tool_choice emitted");
    assert_eq!(tc.get("type").and_then(|v| v.as_str()), Some("function"));
    assert_eq!(tc.get("name").and_then(|v| v.as_str()), Some("get_weather"));
}

#[test]
fn test_responses_tool_choice_absent_is_none() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": "hi"}],
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&json).expect("read_request");
    assert_eq!(ir.tool_choice, None);
    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(out.get("tool_choice").is_none());
}

/// Build a minimal IR request for writer tests, with every Phase-0 / sampling field None/empty so
/// individual tests can set just the one knob under test.
fn empty_ir_request() -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        block_origins: Default::default(),
        reasoning: None,
        reasoning_budgets: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        parallel_tool_calls: None,
        system: Vec::new(),
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop: Vec::new(),
        tool_choice: None,
        stream: false,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        n: None,
        response_format: None,
        extra: serde_json::Map::new(),
        metadata: None,
        service_tier: None,
        store: None,
        safety_identifier: None,
        prompt_cache_key: None,
        verbosity: None,
        allowed_tools: None,
        hosted_tools: Vec::new(),
        system_role: None,
        output_modalities: None,
    }
}

#[test]
fn test_reasoning_item_thinking_round_trip() {
    let body = serde_json::json!({
        "id": "resp_r",
        "status": STATUS_COMPLETED,
        "model": "o3",
        "output": [
            {
                "type": ITEM_TYPE_REASONING,
                "id": "rs_1",
                "summary": [],
                "content": [{"type": CONTENT_TYPE_REASONING_TEXT, "text": "let me think step by step"}],
                "encrypted_content": "ENC_BLOB_123"
            },
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "the answer"}]
            }
        ],
        "usage": {"input_tokens": 5, "output_tokens": 7}
    });
    let reader = ResponsesReader;
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");

    let thinking = ir
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Thinking {
                text, signature, ..
            } => Some((text, signature)),
            _ => None,
        })
        .expect("a Thinking block read from the reasoning item");
    assert_eq!(thinking.0, "let me think step by step");
    assert_eq!(thinking.1.as_deref(), Some("ENC_BLOB_123"));

    let writer = ResponsesWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    let reasoning = out["output"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["type"] == "reasoning"))
        .expect("a reasoning output item written back");
    assert_eq!(
        reasoning["content"][0]["type"],
        "reasoning_text", // golden wire-contract literal (kept bare on purpose)
        "reasoning text part typed reasoning_text"
    );
    assert_eq!(
        reasoning["content"][0]["text"], "let me think step by step",
        "reasoning text round-trips"
    );
    assert_eq!(
        reasoning["encrypted_content"], "ENC_BLOB_123",
        "signature round-trips into encrypted_content"
    );
}

/// Request-input reasoning: a prior-turn `reasoning` INPUT item must read into an IR
/// assistant Thinking block (no longer silently dropped) AND the writer must re-emit it as a
/// top-level `reasoning` input item — a same-protocol Responses->Responses request round-trip.
#[test]
fn reasoning_input_item_round_trips_through_request() {
    let body = serde_json::json!({
        "model": "o3",
        "input": [
            {"role": "user", "content": [{"type": CONTENT_TYPE_INPUT_TEXT, "text": "q"}]},
            {
                "type": ITEM_TYPE_REASONING,
                "id": "rs_in",
                "summary": [],
                "content": [{"type": CONTENT_TYPE_REASONING_TEXT, "text": "prior reasoning"}],
                "encrypted_content": "ENC_IN_42"
            },
            {
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "a"}]
            }
        ]
    });

    let ir = {
        let (_cap, cap_subscriber) = warnings();
        let subscriber = cap_subscriber.clone();
        tracing::dispatcher::with_default(&subscriber, || {
            let reader = ResponsesReader;
            reader.read_request(&body).expect("read_request")
        })
    };

    let thinking = ir
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Thinking {
                text, signature, ..
            } => Some((text, signature)),
            _ => None,
        })
        .expect("a Thinking block decoded from the reasoning input item");
    assert_eq!(thinking.0, "prior reasoning");
    assert_eq!(thinking.1.as_deref(), Some("ENC_IN_42"));

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let reasoning = out["input"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["type"] == "reasoning"))
        .expect("a reasoning input item written back");
    assert_eq!(reasoning["content"][0]["text"], "prior reasoning");
    assert_eq!(reasoning["encrypted_content"], "ENC_IN_42");
}

/// F5 role guard: a `reasoning` INPUT item asserts "this is the model's own prior reasoning" —
/// only an Assistant-role message can truthfully carry that.
#[test]
fn user_role_thinking_is_not_emitted_as_a_reasoning_item() {
    let req = crate::codec::ir::IrRequest {
        system_turns_folded: 0,
        system_folds: Vec::new(),
        messages: vec![crate::codec::ir::IrMessage {
            role: crate::codec::ir::IrRole::User,
            content: vec![crate::codec::ir::IrBlock::Thinking {
                text: "user-authored text masquerading as reasoning".to_string(),
                signature: None,
                redacted: false,
                cache_control: None,
                kind: None,
                signature_origin: None,
            }],
        }],
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);

    let has_reasoning_item = out
        .get("input")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| arr.iter().any(|i| i["type"] == "reasoning"));
    assert!(
        !has_reasoning_item,
        "a User-role Thinking block must never be emitted as a `reasoning` input item: {out}"
    );
}

#[test]
fn test_cached_tokens_mapping() {
    let body = serde_json::json!({
        "id": "resp_c",
        "status": STATUS_COMPLETED,
        "model": "gpt-4o",
        "output": [{
            "type": ITEM_TYPE_MESSAGE,
            "role": "assistant",
            "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi"}]
        }],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 10,
            "input_tokens_details": {"cached_tokens": 64}
        }
    });
    let reader = ResponsesReader;
    let ir = reader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.usage.cache_read_input_tokens,
        Some(64),
        "cached_tokens read into cache_read_input_tokens"
    );

    let writer = ResponsesWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &ir);
    assert_eq!(
        out["usage"]["input_tokens_details"]["cached_tokens"], 64,
        "cache_read_input_tokens written back to cached_tokens"
    );

    let no_cache = serde_json::json!({
        "id": "resp_n", "status": STATUS_COMPLETED, "model": "gpt-4o",
        "output": [{"type":ITEM_TYPE_MESSAGE,"role":"assistant","content":[{"type":CONTENT_TYPE_OUTPUT_TEXT,"text":"x"}]}],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let ir2 = reader
        .read_response(crate::codec::test_host::HOST, &no_cache)
        .expect("read_response");
    assert_eq!(ir2.usage.cache_read_input_tokens, None);
    let out2 = writer.write_response(crate::codec::test_host::HOST, &ir2);

    assert_eq!(
        out2["usage"]["input_tokens_details"]["cached_tokens"], 0,
        "no cache hit => input_tokens_details present with cached_tokens: 0"
    );
}

#[test]
fn test_stop_not_emitted_on_responses() {
    let mut req = empty_ir_request();
    req.messages.push(crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::User,
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
    });
    req.stop = vec!["STOP".to_string(), "END".to_string()];
    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    assert!(
        out.get("stop").is_none(),
        "/v1/responses models no stop param; it must not be emitted: {out}"
    );
    assert!(
        out.get("stop_sequences").is_none(),
        "no stop_sequences either"
    );
}

#[test]
fn test_unsupported_sampling_params_omitted() {
    let mut req = empty_ir_request();
    req.messages.push(crate::codec::ir::IrMessage {
        role: crate::codec::ir::IrRole::User,
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
    });
    req.frequency_penalty = Some(0.5);
    req.presence_penalty = Some(0.3);
    req.seed = Some(42);
    req.n = Some(3);
    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &req);
    for key in ["frequency_penalty", "presence_penalty", "seed", "n"] {
        assert!(
            out.get(key).is_none(),
            "{key} is unsupported on /v1/responses and must be omitted: {out}"
        );
    }
}

#[test]
fn test_response_format_text_format_round_trip() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"role": "user", "content": "hi"}],
        "text": {
            "format": {
                "type": "json_schema",
                "name": "out",
                "schema": {"type": "object"},
                "strict": true
            },
            "verbosity": "low"
        }
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&body).expect("read_request");

    let rf = ir
        .response_format
        .as_ref()
        .expect("response_format promoted");
    assert!(rf.json);
    assert_eq!(rf.name.as_deref(), Some("out"));
    assert_eq!(
        rf.schema.as_ref().and_then(|s| s.get("type")),
        Some(&serde_json::json!("object"))
    );
    assert_eq!(rf.strict, Some(true));

    assert_eq!(
        ir.extra.get("text").and_then(|t| t.get("verbosity")),
        Some(&serde_json::json!("low")),
        "text.verbosity preserved in extra"
    );

    assert!(
        ir.extra.get("text").and_then(|t| t.get("format")).is_none(),
        "text.format must be promoted, not left in extra"
    );

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let fmt = &out["text"]["format"];
    assert_eq!(fmt["type"], "json_schema", "flat text.format type");
    assert_eq!(fmt["name"], "out", "name flattened beside type");
    assert_eq!(fmt["schema"]["type"], "object");
    assert_eq!(fmt["strict"], true);
    assert!(
        fmt.get("json_schema").is_none(),
        "Responses text.format is FLAT, no nested json_schema key: {fmt}"
    );
    assert_eq!(
        out["text"]["verbosity"], "low",
        "verbosity merged alongside format"
    );
}

#[test]
fn test_input_image_file_id_round_trip() {
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [{
            "type": "input_image",
            "file_id": "file-abc123"
        }]
    });
    let reader = ResponsesReader;
    let ir = reader.read_request(&body).expect("read_request");
    let file_id = ir
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Image {
                source: crate::codec::ir::IrImageSource::Vendor { vendor, value },
                ..
            } if *vendor == "responses" => value
                .get("file_id")
                .and_then(|i| i.as_str())
                .map(String::from),
            _ => None,
        })
        .expect("a responses-vendor Image block from the file_id image");
    assert_eq!(file_id, "file-abc123", "file_id preserved");

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let image_item = out["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(|c| c.as_array()))
        .flatten()
        .find(|c| c["type"] == "input_image")
        .expect("an input_image content block written back");
    assert_eq!(image_item["file_id"], "file-abc123", "file_id round-trips");
    assert!(
        image_item.get("image_url").is_none(),
        "a file_id image must not gain a spurious image_url: {image_item}"
    );
}

#[test]
fn test_streaming_reasoning_round_trip() {
    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();

    let added = reader.read_response_events(
        crate::codec::test_host::HOST,
        EVT_OUTPUT_ITEM_ADDED,
        &serde_json::json!({
            "output_index": 0,
            "item": {"type": ITEM_TYPE_REASONING, "id": "rs_1"}
        }),
        &mut state,
    );
    assert!(
        added.iter().any(|e| matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: _,
            }
        )),
        "reasoning item.added opens a Thinking block at index 0: {added:?}"
    );

    let delta = reader.read_response_events(
        crate::codec::test_host::HOST,
        EVT_REASONING_TEXT_DELTA,
        &serde_json::json!({"output_index": 0, "delta": "pondering"}),
        &mut state,
    );
    assert!(
        delta.iter().any(|e| matches!(
            e,
            crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::ThinkingDelta(t)
            } if t == "pondering"
        )),
        "reasoning_text.delta yields a ThinkingDelta: {delta:?}"
    );

    let writer = ResponsesWriter;
    let (etype, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockStart {
                index: 0,
                block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                refusal: false,
            },
        )
        .expect("Thinking BlockStart emits a frame");
    assert_eq!(etype, "response.output_item.added");
    assert_eq!(payload["item"]["type"], "reasoning");

    let (etype2, payload2) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::ThinkingDelta("pondering".to_string()),
            },
        )
        .expect("ThinkingDelta emits a frame");
    assert_eq!(etype2, "response.reasoning_text.delta");
    assert_eq!(payload2["delta"], "pondering");

    let close_frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    );
    let close_types: Vec<&str> = close_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        close_types,
        vec!["response.reasoning_text.done", "response.output_item.done"], // golden wire-contract literals (kept bare on purpose)
        "reasoning BlockStop closes reasoning_text then output_item"
    );

    assert_eq!(close_frames[0].1["text"], "pondering");
    assert_eq!(close_frames[0].1["content_index"], 0);
    let payload3 = &close_frames[1].1;
    assert_eq!(payload3["item"]["type"], "reasoning");
    assert_eq!(payload3["item"]["content"][0]["text"], "pondering");
}

#[test]
fn test_streaming_cached_tokens_round_trip() {
    let reader = ResponsesReader;
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let events = reader.read_response_events(
        crate::codec::test_host::HOST,
        EVT_RESPONSE_COMPLETED,
        &serde_json::json!({
            "response": {
                "status": STATUS_COMPLETED,
                "usage": {
                    "input_tokens": 50,
                    "output_tokens": 5,
                    "input_tokens_details": {"cached_tokens": 32}
                }
            }
        }),
        &mut state,
    );
    let usage = events
        .iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("a MessageDelta with usage");
    assert_eq!(usage.cache_read_input_tokens, Some(32));

    let writer = ResponsesWriter;
    let (_etype, payload) = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &crate::codec::ir::IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: crate::codec::ir::IrUsage {
                    input_tokens: 50,
                    output_tokens: 5,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: Some(32),
                    detail: crate::codec::ir::IrUsageDetail::default(),
                },
                stop_detail: None,
            },
        )
        .expect("MessageDelta emits a terminal frame");
    assert_eq!(
        payload["response"]["usage"]["input_tokens_details"]["cached_tokens"], 32,
        "streamed terminal re-emits cached_tokens: {payload}"
    );
}

/// Non-stream `read_response` cache-token NORMALIZATION (ir.rs:457): the Responses API's
/// `input_tokens` is a TOTAL that INCLUDES the cached prefix, so the reader SUBTRACTS
/// `input_tokens_details.cached_tokens` to leave `input_tokens` UNCACHED and stores the cached
/// count additively in `cache_read_input_tokens` (the OpenAI-family convention).
#[test]
fn read_response_subtracts_cached_prefix_from_input_tokens() {
    let body = serde_json::json!({
        "id": "resp_abc",
        "object": OBJ_RESPONSE,
        "created_at": 1_700_000_000_u64,
        "status": STATUS_COMPLETED,
        "model": "gpt-4o",
        "output": [{
            "type": ITEM_TYPE_MESSAGE,
            "role": "assistant",
            "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "hi"}]
        }],
        "usage": {
            "input_tokens": 150,
            "output_tokens": 5,
            "input_tokens_details": {"cached_tokens": 100}
        }
    });
    let resp = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        resp.usage.input_tokens, 50,
        "input_tokens must be wire input(150) MINUS cached(100) = 50 (uncached)"
    );
    assert_eq!(resp.usage.cache_read_input_tokens, Some(100));
    assert_eq!(resp.usage.output_tokens, 5);

    assert_eq!(resp.usage.billable_tokens(), 155);
}

/// The write side RECONSTRUCTS the wire shape: it ADDS the cached prefix BACK onto the uncached IR
/// `input_tokens` to re-derive the Responses TOTAL `input_tokens`, and re-emits the FLAT
/// `input_tokens_details.cached_tokens`.
#[test]
fn write_response_reconstructs_input_tokens_total_with_cached_details() {
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        content: vec![crate::codec::ir::IrBlock::text("hi".to_string())],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        usage: crate::codec::ir::IrUsage {
            input_tokens: 50,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(100),
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        model: Some("gpt-4o".to_string()),
        id: Some("resp_abc".to_string()),
        created: Some(1_700_000_000),
        system_fingerprint: None,
        stop_sequence: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };
    let writer = ResponsesWriter;
    let out = writer.write_response(crate::codec::test_host::HOST, &resp);
    assert_eq!(
        out["usage"]["input_tokens"],
        serde_json::json!(150),
        "input_tokens must re-add the cached prefix: uncached(50) + cached(100) = 150"
    );
    assert_eq!(out["usage"]["output_tokens"], serde_json::json!(5));
    assert_eq!(
        out["usage"]["input_tokens_details"]["cached_tokens"],
        serde_json::json!(100)
    );
}

/// `incomplete_details.reason` normalization must be EXHAUSTIVE (ir.rs:186): `max_output_tokens`→
/// MaxTokens, `content_filter`→Safety, `refusal`→Refusal, and any unknown/future reason→Other (no
/// String payload).
#[test]
fn incomplete_reason_codec_is_exhaustive() {
    use crate::codec::ir::IrStopReason as S;
    assert_eq!(
        read_responses_incomplete_reason(INCOMPLETE_REASON_MAX_OUTPUT),
        S::MaxTokens
    );
    assert_eq!(
        read_responses_incomplete_reason(INCOMPLETE_REASON_CONTENT_FILTER),
        S::Safety
    );
    assert_eq!(read_responses_incomplete_reason("refusal"), S::Refusal);
    assert_eq!(
        read_responses_incomplete_reason("some_future_reason"),
        S::Other
    );

    assert_eq!(write_responses_status(S::MaxTokens), STATUS_INCOMPLETE);
    assert_eq!(write_responses_status(S::Safety), STATUS_INCOMPLETE);
    assert_eq!(write_responses_status(S::ToolUse), STATUS_COMPLETED);
    assert_eq!(write_responses_status(S::EndTurn), STATUS_COMPLETED);
    assert_eq!(
        write_responses_incomplete_reason(S::MaxTokens),
        INCOMPLETE_REASON_MAX_OUTPUT
    );
    assert_eq!(
        write_responses_incomplete_reason(S::Safety),
        INCOMPLETE_REASON_CONTENT_FILTER
    );
    assert_eq!(
        write_responses_incomplete_reason(S::Other),
        INCOMPLETE_REASON_OTHER
    );
}

/// A full `read_response` with `status:"incomplete"` + `incomplete_details.reason:"content_filter"`
#[test]
fn read_response_incomplete_details_promote_stop_reason() {
    let mk = |reason: &str| {
        serde_json::json!({
            "id": "resp_x",
            "object": OBJ_RESPONSE,
            "created_at": 1,
            "status": STATUS_INCOMPLETE,
            "model": "gpt-4o",
            "incomplete_details": {"reason": reason},
            "output": [{
                "type": ITEM_TYPE_MESSAGE,
                "role": "assistant",
                "content": [{"type": CONTENT_TYPE_OUTPUT_TEXT, "text": "partial"}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })
    };
    assert_eq!(
        ResponsesReader
            .read_response(
                crate::codec::test_host::HOST,
                &mk(INCOMPLETE_REASON_CONTENT_FILTER)
            )
            .expect("read")
            .stop_reason,
        Some(crate::codec::ir::IrStopReason::Safety)
    );
    assert_eq!(
        ResponsesReader
            .read_response(
                crate::codec::test_host::HOST,
                &mk(INCOMPLETE_REASON_MAX_OUTPUT)
            )
            .expect("read")
            .stop_reason,
        Some(crate::codec::ir::IrStopReason::MaxTokens)
    );
}

/// A `response_format` json_schema round-trips read→IR→write through the Responses FLAT
/// `text.format` shape (name/schema/strict/description sit BESIDE `type`, not nested under
/// `json_schema` like OpenAI Chat).
#[test]
fn text_format_json_schema_flat_round_trips() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
    });
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "text": {"format": {
            "type": "json_schema",
            "name": "Answer",
            "schema": schema,
            "strict": true,
            "description": "an answer"
        }}
    });
    let ir = ResponsesReader.read_request(&body).expect("read_request");
    let rf = ir
        .response_format
        .as_ref()
        .expect("text.format canonicalizes into the typed IR");
    assert!(rf.json);
    assert_eq!(rf.name.as_deref(), Some("Answer"));
    assert_eq!(rf.strict, Some(true));
    assert_eq!(rf.description.as_deref(), Some("an answer"));
    assert_eq!(rf.schema.as_ref(), Some(&schema));

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let fmt = out
        .pointer("/text/format")
        .expect("text.format must be emitted");
    assert_eq!(
        fmt.get("type").and_then(|t| t.as_str()),
        Some("json_schema")
    );
    assert_eq!(fmt.get("name").and_then(|n| n.as_str()), Some("Answer"));
    assert_eq!(fmt.get("schema"), Some(&schema));
    assert_eq!(fmt.get("strict").and_then(|s| s.as_bool()), Some(true));

    assert!(
        fmt.get("json_schema").is_none(),
        "Responses text.format is FLAT — must not nest under json_schema: {out}"
    );
}

/// Hosted tools: a Responses request whose `tools` array mixes a HOSTED tool
/// (`web_search`, `file_search`) with a CUSTOM function tool must round-trip the hosted specs
/// — never mangled into empty `{"type":"function","name":""}` tools — while the function
/// tool keeps its flat Responses shape.
#[test]
fn test_hosted_tools_pass_through_intact() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": "search the web",
        "tools": [
            {"type": "web_search", "search_context_size": "medium"},
            {"type": "file_search", "vector_store_ids": ["vs_123"]},
            {
                "type": "function",
                "name": "get_weather",
                "description": "look up the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        ]
    });

    let ir = ResponsesReader.read_request(&json).expect("reads");

    assert_eq!(ir.tools.len(), 2);
    assert!(
        ir.tools[0].hosted.is_some(),
        "file_search must be a hosted passthrough"
    );
    assert!(
        ir.tools[1].hosted.is_none(),
        "the function tool must NOT be hosted"
    );
    assert_eq!(ir.tools[1].name, "get_weather");
    assert_eq!(
        ir.hosted_tools,
        vec![crate::codec::ir::IrHostedTool::WebSearch(
            crate::codec::ir::IrWebSearch {
                search_context_size: Some(crate::codec::ir::IrVerbosity::Medium),
                ..Default::default()
            }
        )],
        "web_search rides the neutral hosted slot"
    );

    let writer = ResponsesWriter;
    let out = writer.write_request(crate::codec::test_host::HOST, &ir);
    let tools = out["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 3);
    let by_type = |t: &str| {
        tools
            .iter()
            .find(|x| x["type"] == t)
            .unwrap_or_else(|| panic!("no {t} tool: {out}"))
    };

    let web_search = by_type("web_search");
    assert_eq!(web_search["search_context_size"], "medium");
    assert!(
        web_search.get("name").is_none() && web_search.get("parameters").is_none(),
        "a hosted tool must NOT gain function-tool fields: {web_search}"
    );
    assert_eq!(
        by_type("file_search")["vector_store_ids"],
        serde_json::json!(["vs_123"])
    );

    let function = by_type("function");
    assert_eq!(function["name"], "get_weather");
    assert!(function.get("parameters").is_some());

    for t in tools {
        if t["type"] == "function" {
            assert_ne!(
                t["name"], "",
                "no function tool may carry an empty name: {t}"
            );
        }
    }
}

/// Hosted tools, CROSS-protocol drop: a Responses hosted tool routed to a NON-Responses egress
/// has no function-tool analog, so `IrReq::prepare_for_egress` (the cross-protocol seam) must DROP it
/// with a warn - never let a non-Responses writer emit a malformed empty-name function tool the
/// backend 400s on.
#[test]
fn test_hosted_tools_dropped_cross_protocol() {
    let json = serde_json::json!({
        "model": "gpt-4o",
        "input": "search the web",
        "tools": [
            // A raw hosted tool (IR-11 models no neutral file search; a `web_search` now crosses
            // in `hosted_tools` instead).
            {"type": "file_search", "vector_store_ids": ["vs_123"]},
            {
                "type": "function",
                "name": "get_weather",
                "description": "look up the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        ]
    });
    let ir = ResponsesReader.read_request(&json).expect("reads");
    assert_eq!(ir.tools.len(), 2, "reader keeps hosted + function tool");
    assert!(ir.tools[0].hosted.is_some(), "file_search is hosted");

    let mut req = ir;
    super::super::chat_handle::chat_prepare_for_egress(
        &mut req,
        &crate::codec::egress_prep::EgressPrep {
            thought_signature_fill: false,
            ingress_protocol: "openai-responses",
            egress_requires_max_tokens: false,
            lane_default_max_tokens: None,
            global_default_max_tokens: 4096,
            reasoning_allowed: false,
            reasoning_budgets: [0, 0, 0, 0],
            prompt_caching_allowed: true,
            cache_control_cap: None,
            lane_caps: Default::default(),
        },
    );
    let ir = req;

    assert_eq!(ir.tools.len(), 1, "hosted tool dropped, function tool kept");
    assert!(
        ir.tools.iter().all(|t| t.hosted.is_none()),
        "no hosted tool may survive the cross-protocol seam"
    );
    assert_eq!(ir.tools[0].name, "get_weather");

    let out = super::super::openai_chat::openai_writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    let tools = out["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "only the function tool is written: {out}");
    for t in tools {
        assert_ne!(
            t["function"]["name"], "",
            "no empty-name function tool may reach a non-Responses backend: {t}"
        );
    }

    fn any_empty_name(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                if m.get("name") == Some(&serde_json::json!("")) {
                    return true;
                }
                m.values().any(any_empty_name)
            }
            serde_json::Value::Array(a) => a.iter().any(any_empty_name),
            _ => false,
        }
    }
    fn mentions_name(v: &serde_json::Value, needle: &str) -> bool {
        match v {
            serde_json::Value::Object(m) => m
                .iter()
                .any(|(k, val)| (k == "name" && val == needle) || mentions_name(val, needle)),
            serde_json::Value::Array(a) => a.iter().any(|e| mentions_name(e, needle)),
            _ => false,
        }
    }

    let anthropic = super::super::anthropic::anthropic_writer()
        .write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        !any_empty_name(&anthropic),
        "Anthropic egress must not emit an empty-name tool: {anthropic}"
    );
    assert!(
        mentions_name(&anthropic, "get_weather"),
        "Anthropic egress must keep the surviving function tool: {anthropic}"
    );

    let gemini_writer = super::super::gemini::GeminiWriter;
    let gemini = gemini_writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        !any_empty_name(&gemini),
        "Gemini egress must not emit an empty-name tool: {gemini}"
    );
    assert!(
        mentions_name(&gemini, "get_weather"),
        "Gemini egress must keep the surviving function tool: {gemini}"
    );

    let cohere_writer = super::super::cohere::CohereWriter;
    let cohere = cohere_writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        !any_empty_name(&cohere),
        "Cohere egress must not emit an empty-name tool: {cohere}"
    );
    assert!(
        mentions_name(&cohere, "get_weather"),
        "Cohere egress must keep the surviving function tool: {cohere}"
    );

    let bedrock_writer = super::super::bedrock::BedrockWriter;
    let bedrock = bedrock_writer.write_request(crate::codec::test_host::HOST, &ir);
    assert!(
        !any_empty_name(&bedrock),
        "Bedrock egress must not emit an empty-name tool: {bedrock}"
    );
    assert!(
        mentions_name(&bedrock, "get_weather"),
        "Bedrock egress must keep the surviving function tool: {bedrock}"
    );
}

/// Usage required fields: the emitted Responses `usage` object must carry `total_tokens`
#[test]
fn test_responses_usage_carries_all_required_fields() {
    let no_cache = crate::codec::ir::IrUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    let u = super::build_responses_usage(&no_cache);
    assert_eq!(u["input_tokens"], 10);
    assert_eq!(u["output_tokens"], 5);
    assert_eq!(
        u["total_tokens"], 15,
        "total_tokens is required and must be input+output"
    );
    assert_eq!(
        u["input_tokens_details"]["cached_tokens"], 0,
        "input_tokens_details.cached_tokens is required (0 when no cache): {u}"
    );
    assert_eq!(
        u["output_tokens_details"]["reasoning_tokens"], 0,
        "output_tokens_details.reasoning_tokens is required (0 when non-reasoning): {u}"
    );

    let cached = crate::codec::ir::IrUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_input_tokens: Some(3),
        cache_read_input_tokens: Some(7),
        detail: crate::codec::ir::IrUsageDetail::default(),
    };
    let uc = super::build_responses_usage(&cached);
    assert_eq!(
        uc["input_tokens"], 20,
        "input_tokens is the cache-inclusive total (10+7+3)"
    );
    assert_eq!(uc["input_tokens_details"]["cached_tokens"], 7);
    assert_eq!(
        uc["total_tokens"], 25,
        "total = input_total(20) + output(5)"
    );

    assert_eq!(
        uc["input_tokens_details"]["cache_write_tokens"], 3,
        "cache_write_tokens is a required Responses usage member carrying the cache-creation count: {uc}"
    );
}

/// EVERY member the pinned OpenAI spec marks REQUIRED on a `Response` object — including the
/// request-echo members the IR cannot carry (`instructions`, `tools`, `tool_choice`,
/// `parallel_tool_calls`, `metadata`, `temperature`, `top_p`) and the nullable
/// `incomplete_details` — must be PRESENT on the non-stream body and on the inner `response` of
/// every lifecycle event (`response.created`, `response.completed`, `response.failed`), with the
/// spec's default values where the IR carries nothing; and every `output_text` part (and the
/// streaming `output_text.delta`/`.done` events) must carry `logprobs`.
#[test]
fn responses_spec_required_members_present_on_every_response_shape() {
    fn assert_required(resp: &serde_json::Value, ctx: &str) {
        for key in [
            "id",
            "object",
            "created_at",
            "error",
            "incomplete_details",
            "instructions",
            "model",
            "tools",
            "output",
            "parallel_tool_calls",
            "metadata",
            "tool_choice",
            "temperature",
            "top_p",
        ] {
            assert!(
                resp.get(key).is_some(),
                "{ctx}: spec-required member `{key}` missing: {resp}"
            );
        }
        assert_eq!(resp["instructions"], serde_json::Value::Null, "{ctx}");
        assert_eq!(resp["tools"], serde_json::json!([]), "{ctx}");
        assert_eq!(resp["tool_choice"], "auto", "{ctx}");
        assert_eq!(resp["parallel_tool_calls"], true, "{ctx}");
        assert_eq!(resp["metadata"], serde_json::json!({}), "{ctx}");
        assert_eq!(resp["temperature"], 1.0, "{ctx}");
        assert_eq!(resp["top_p"], 1.0, "{ctx}");
        if let Some(usage) = resp.get("usage") {
            assert!(usage.is_object(), "{ctx}: usage must be an object: {usage}");
            assert!(
                usage["input_tokens_details"]
                    .get("cache_write_tokens")
                    .is_some(),
                "{ctx}: usage.input_tokens_details.cache_write_tokens required: {usage}"
            );
        }
        for item in resp["output"].as_array().unwrap() {
            if item["type"] == "message" {
                for part in item["content"].as_array().unwrap() {
                    assert_eq!(
                        part["logprobs"],
                        serde_json::json!([]),
                        "{ctx}: output_text part must carry logprobs: {part}"
                    );
                }
            }
        }
    }

    let non_stream = ResponsesWriter;
    let body = non_stream.write_response(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrResponse {
            role: crate::codec::ir::IrRole::Assistant,
            content: vec![crate::codec::ir::IrBlock::text("hi")],
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            usage: usage_fixture(),
            model: None,
            id: None,
            created: None,
            system_fingerprint: None,
            stop_sequence: None,
            logprobs: Vec::new(),

            request_echo: None,
            stop_detail: None,
            ..Default::default()
        },
    );
    assert_required(&body, "non-stream body");
    assert_eq!(body["incomplete_details"], serde_json::Value::Null);

    let truncated = non_stream.write_response(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrResponse {
            role: crate::codec::ir::IrRole::Assistant,
            content: Vec::new(),
            stop_reason: Some(crate::codec::ir::IrStopReason::MaxTokens),
            usage: usage_fixture(),
            model: None,
            id: None,
            created: None,
            system_fingerprint: None,
            stop_sequence: None,
            logprobs: Vec::new(),

            request_echo: None,
            stop_detail: None,
            ..Default::default()
        },
    );
    assert_required(&truncated, "non-stream incomplete body");
    assert_eq!(
        truncated["incomplete_details"]["reason"],
        "max_output_tokens"
    );

    let writer = ResponsesWriter;
    let created = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("created");
    assert_required(&created.1["response"], "response.created");

    let opened = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );
    assert_eq!(opened[1].0, "response.content_part.added");
    assert_eq!(opened[1].1["part"]["logprobs"], serde_json::json!([]));
    let delta = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index: 0,
                delta: crate::codec::ir::IrDelta::TextDelta("hi".into()),
            },
        )
        .expect("delta");
    assert_eq!(delta.1["logprobs"], serde_json::json!([]));
    let closed = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    assert_eq!(closed[0].0, "response.output_text.done");
    assert_eq!(closed[0].1["logprobs"], serde_json::json!([]));
    assert_eq!(closed[1].0, "response.content_part.done");
    assert_eq!(closed[1].1["part"]["logprobs"], serde_json::json!([]));
    assert_eq!(closed[2].0, "response.output_item.done");
    assert_eq!(
        closed[2].1["item"]["content"][0]["logprobs"],
        serde_json::json!([])
    );

    let completed = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageDelta {
                stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
                stop_sequence: None,
                usage: usage_fixture(),
                stop_detail: None,
            },
        )
        .expect("completed");
    assert_required(&completed.1["response"], "response.completed");

    let failed = writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("boom".to_string()),
                retry_after: None,
            }),
        )
        .expect("failed");
    assert_required(&failed.1["response"], "response.failed");
}

/// Streamed token logprobs (an IR `LogprobsDelta`) are buffered and emitted on the
/// `output_text.done` event (event shape: token/logprob/top_logprobs) and on the finalized
/// `output_text` part (part shape: token/logprob/bytes/top_logprobs), instead of being dropped.
#[test]
fn responses_stream_logprobs_ride_text_done_and_finalized_part() {
    let writer = ResponsesWriter;
    writer
        .write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: None,
                created: None,
                model: None,
            },
        )
        .expect("created");
    writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
    );
    writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("Hi".into()),
        },
    );
    writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::LogprobsDelta(vec![
                crate::codec::ir::IrTokenLogprob {
                    token: "Hi".into(),
                    logprob: -0.25,
                    bytes: None,
                    bytes_null: false,
                    top: vec![crate::codec::ir::IrTopLogprob {
                        token: "Hello".into(),
                        logprob: -1.5,
                        bytes: None,
                        bytes_null: false,
                    }],
                },
            ]),
        },
    );
    let closed = writer.write_response_events(
        crate::codec::test_host::HOST,
        &IrStreamEvent::BlockStop { index: 0 },
    );
    assert_eq!(
        closed[0].1["logprobs"],
        serde_json::json!([{
            "token": "Hi",
            "logprob": -0.25,
            "top_logprobs": [{ "token": "Hello", "logprob": -1.5 }]
        }]),
        "output_text.done carries the event-shape logprobs"
    );
    let part = &closed[1].1["part"]["logprobs"];
    assert_eq!(part[0]["token"], "Hi");
    assert_eq!(part[0]["logprob"], -0.25);
    assert_eq!(part[0]["bytes"], serde_json::json!([72, 105]));
    assert_eq!(part[0]["top_logprobs"][0]["token"], "Hello");
    assert_eq!(
        closed[2].1["item"]["content"][0]["logprobs"], *part,
        "the finalized item part carries the same logprobs as content_part.done"
    );
}

/// Grounding citations were dropped on the way into Responses: the writer hardcoded
/// `annotations: []`, so an Anthropic web-search or Gemini grounding response arrived at an
/// OpenAI-dialect client with its sources gone.
#[test]
fn write_response_emits_url_citations_without_inventing_fields() {
    let cite = |kind: &str,
                cited_text: Option<&str>,
                title: Option<&str>,
                url: Option<&str>,
                start: Option<i64>,
                end: Option<i64>| crate::codec::ir::IrCitation {
        domain: None,
        kind: Some(kind.to_string()),
        cited_text: cited_text.map(str::to_string),
        title: title.map(str::to_string),
        url: url.map(str::to_string),
        document_index: None,
        start_index: start,
        end_index: end,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };

    let text = "The tide turns at dawn. Nothing else is known.";
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        id: Some("resp_c".to_string()),
        model: Some("gpt-4o".to_string()),
        created: Some(1_700_000_000),
        content: vec![crate::codec::ir::IrBlock::Text {
            text: text.to_string(),
            cache_control: None,
            citations: vec![
                // Anthropic web search: url + title, NO offsets. Recoverable from the quote.
                cite(
                    "web_search_result_location",
                    Some("The tide turns at dawn."),
                    Some("Tide Tables"),
                    Some("https://example.com/tides"),
                    None,
                    None,
                ),
                // Gemini grounding: url + offsets, NO title. The url stands in for the title.
                cite(
                    "web_search_result_location",
                    None,
                    None,
                    Some("https://example.org/almanac"),
                    Some(24),
                    Some(45),
                ),
                // No url and no recoverable span: a document reference is a different wire shape
                // keyed by a file_id the IR does not carry, so it is omitted, not mis-shaped.
                cite(
                    "char_location",
                    None,
                    Some("Internal Memo"),
                    None,
                    Some(0),
                    Some(4),
                ),
                // A quote that does not occur in the text: the anchor would have to be guessed.
                cite(
                    "web_search_result_location",
                    Some("no such sentence"),
                    Some("Ghost"),
                    Some("https://example.net/ghost"),
                    None,
                    None,
                ),
            ],
            refusal: false,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        system_fingerprint: None,

        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let writer = ResponsesWriter;
    let v = writer.write_response(crate::codec::test_host::HOST, &resp);
    let anns = v["output"][0]["content"][0]["annotations"]
        .as_array()
        .expect("annotations array");

    assert_eq!(
        anns.len(),
        2,
        "only the honestly-mappable citations: {anns:?}"
    );
    assert_eq!(anns[0]["type"], "url_citation");
    assert_eq!(anns[0]["url"], "https://example.com/tides");
    assert_eq!(anns[0]["title"], "Tide Tables");
    assert_eq!(anns[0]["start_index"], 0, "span recovered from the quote");
    assert_eq!(anns[0]["end_index"], 23);
    assert_eq!(
        anns[1]["title"], "https://example.org/almanac",
        "a source with no title falls back to its url, never a placeholder"
    );
    assert_eq!(anns[1]["start_index"], 24);
}

/// The streaming path dropped citations outright: Responses carries them as `annotations` on the
/// assembled `output_text` part, not as a delta frame, so a `CitationsDelta` had nowhere to go at
/// arrival time and was discarded instead of buffered until `BlockStop` built that part.
#[test]
fn streamed_citations_reach_the_assembled_output_item() {
    let w = ResponsesWriter;
    let ev = |e: crate::codec::ir::IrStreamEvent| {
        w.write_response_event(crate::codec::test_host::HOST, &e)
    };

    let _ = ev(crate::codec::ir::IrStreamEvent::BlockStart {
        index: 0,
        block: crate::codec::ir::IrBlockMeta::Text,
        refusal: false,
    });
    let _ = ev(crate::codec::ir::IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::TextDelta("Sourced claim.".into()),
    });
    let _ = ev(crate::codec::ir::IrStreamEvent::BlockDelta {
        index: 0,
        delta: crate::codec::ir::IrDelta::CitationsDelta(vec![crate::codec::ir::IrCitation {
            domain: None,
            kind: Some("web_search_result_location".into()),
            cited_text: Some("Sourced claim.".into()),
            title: Some("A Source".into()),
            url: Some("https://example.com/s".into()),
            document_index: None,
            start_index: None,
            end_index: None,
            encrypted_index: None,
            raw: None,
            ..Default::default()
        }]),
    });
    let close_frames = w.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    );
    let close_types: Vec<&str> = close_frames.iter().map(|(et, _)| et.as_str()).collect();
    assert_eq!(
        close_types,
        vec![
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done"
        ],
        "text close is the three-frame bracket"
    );

    let part_anns = close_frames[1].1["part"]["annotations"]
        .as_array()
        .expect("content_part.done annotations array");
    assert_eq!(
        part_anns.len(),
        1,
        "citation survives onto the content part"
    );
    assert_eq!(part_anns[0]["url"], "https://example.com/s");

    let anns = close_frames[2].1["item"]["content"][0]["annotations"]
        .as_array()
        .expect("annotations array");
    assert_eq!(
        anns.len(),
        1,
        "the streamed citation must survive: {anns:?}"
    );
    assert_eq!(anns[0]["url"], "https://example.com/s");
    assert_eq!(anns[0]["end_index"], 14);
}

/// STRICT-CLIENT CONTRACT: the Responses egress writer, driven with the IR event stream a
/// CROSS-protocol egress produces (an Anthropic/Gemini backend re-framed to `/v1/responses` — the
/// shape a Codex CLI client consumes), must emit the intermediate `response.content_part.added`
/// between `output_item.added` and the first `output_text.delta`, and must close the text part with
/// `output_text.done` → `content_part.done` before `output_item.done`.
#[test]
fn cross_protocol_egress_into_responses_emits_content_part_bracket() {
    let writer = ResponsesWriter;

    let ir_stream = [
        IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            usage: None,
            id: None,
            created: None,
            model: None,
        },
        IrStreamEvent::BlockStart {
            index: 0,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: false,
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("Hel".to_string()),
        },
        IrStreamEvent::BlockDelta {
            index: 0,
            delta: crate::codec::ir::IrDelta::TextDelta("lo".to_string()),
        },
        IrStreamEvent::BlockStop { index: 0 },
        IrStreamEvent::MessageDelta {
            stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
            stop_sequence: None,
            usage: usage_fixture(),
            stop_detail: None,
        },
    ];

    let frames: Vec<serde_json::Value> = ir_stream
        .iter()
        .flat_map(|ev| writer.write_response_events(crate::codec::test_host::HOST, ev))
        .map(|(_et, data)| data)
        .collect();
    let types: Vec<String> = frames
        .iter()
        .map(|f| {
            f["type"]
                .as_str()
                .expect("every frame carries a type")
                .to_string()
        })
        .collect();

    assert_eq!(
        types,
        vec![
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ],
        "cross-protocol egress into Responses must carry the full content-part bracket, got {types:?}"
    );

    let pos = |name: &str| types.iter().position(|t| t == name).unwrap();
    assert!(
        pos("response.content_part.added") > pos("response.output_item.added"),
        "content_part.added must follow output_item.added"
    );
    assert!(
        pos("response.content_part.added") < pos("response.output_text.delta"),
        "content_part.added must PRECEDE the first output_text.delta (else a strict client drops it)"
    );

    assert!(pos("response.output_text.done") < pos("response.content_part.done"));
    assert!(pos("response.content_part.done") < pos("response.output_item.done"));

    let msg_id = frames[1]["item_id"]
        .as_str()
        .expect("output_item.added item_id");
    for name in [
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.content_part.done",
        "response.output_item.done",
    ] {
        let f = &frames[pos(name)];
        assert_eq!(
            f["item_id"].as_str(),
            Some(msg_id),
            "{name} must carry the same msg_ item_id as output_item.added"
        );
    }
    for name in [
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.content_part.done",
    ] {
        assert_eq!(
            frames[pos(name)]["content_index"],
            0,
            "{name} content_index"
        );
    }

    let seqs: Vec<u64> = frames
        .iter()
        .map(|f| f["sequence_number"].as_u64().expect("sequence_number"))
        .collect();
    let expected_seqs: Vec<u64> = (0..frames.len() as u64).collect();
    assert_eq!(
        seqs, expected_seqs,
        "sequence_number must be a gap-free 0..N run across all frames, got {seqs:?}"
    );
}

/// A `developer`/`system` item AFTER an earlier conversational turn is still hoisted
/// (behavior unchanged — the IR cannot express a positioned system turn), but must now WARN, since
/// hoisting silently changes when the instruction takes effect relative to native Responses
/// semantics.
#[test]
fn mid_conversation_developer_item_is_flagged() {
    let body = serde_json::json!({
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "message", "role": "assistant", "content": "hello"},
            {"type": "message", "role": "developer", "content": "now answer in French"},
            {"type": "message", "role": "user", "content": "and again"}
        ]
    });

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let ir = tracing::dispatcher::with_default(&subscriber, || {
        ResponsesReader.read_request(&body).expect("read_request")
    });

    assert!(
        ir.system.iter().any(|b| matches!(
            b,
            crate::codec::ir::IrBlock::Text { text, .. } if text.contains("now answer in French")
        )),
        "the developer item's text must still be hoisted into system: {:?}",
        ir.system
    );
    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("appeared AFTER an earlier conversational turn")),
        "a developer/system item after a conversational turn must warn about the hoist: {:?}",
        diagnostic_texts(&cap)
    );
}

/// Negative half (REGRESSION PROOF): a `developer`/`system` item BEFORE any conversational turn is
/// the ordinary shape and must NOT warn.
#[test]
fn leading_developer_item_does_not_warn() {
    let body = serde_json::json!({
        "input": [
            {"type": "message", "role": "developer", "content": "be terse"},
            {"type": "message", "role": "user", "content": "hi"}
        ]
    });

    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let _ir = tracing::dispatcher::with_default(&subscriber, || {
        ResponsesReader.read_request(&body).expect("read_request")
    });

    assert!(
        !diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("appeared AFTER an earlier conversational turn")),
        "a leading developer item must not warn: {:?}",
        diagnostic_texts(&cap)
    );
}

/// A Responses `output_text` content part's sibling `annotations` (`url_citation` entries) must be
/// read into the IR Text block's `citations`, including its URL, title and character offsets.
#[test]
fn responses_output_text_annotations_read_into_ir_citations() {
    let json = serde_json::json!({
        "id": "resp_cite1",
        "object": OBJ_RESPONSE,
        "status": STATUS_COMPLETED,
        "output": [{
            "type": ITEM_TYPE_MESSAGE,
            "role": "assistant",
            "content": [{
                "type": CONTENT_TYPE_OUTPUT_TEXT,
                "text": "See the source for details.",
                "annotations": [{
                    "type": "url_citation",
                    "url": "https://example.com/r",
                    "title": "Responses Source",
                    "start_index": 7,
                    "end_index": 20
                }]
            }]
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let resp = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response should succeed");
    let citations = resp
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations),
            _ => None,
        })
        .expect("a Text block must be present");
    assert_eq!(citations.len(), 1, "{citations:?}");
    let c = &citations[0];
    assert_eq!(c.url.as_deref(), Some("https://example.com/r"));
    assert_eq!(c.title.as_deref(), Some("Responses Source"));

    assert_eq!(
        (c.start_index, c.end_index),
        (Some(7), Some(20)),
        "the url_citation span must carry: {c:?}"
    );
}

/// Exactly one text-bearing part — whether it lives in `content[]` alone or `summary[]` alone —
/// must borrow straight from the item, not allocate.
#[test]
fn read_reasoning_text_single_part_borrows() {
    let content_only = serde_json::json!({
        "type": "reasoning",
        "content": [{"type": "reasoning_text", "text": "one part"}]
    });
    assert!(matches!(
        read_reasoning_text(&content_only),
        std::borrow::Cow::Borrowed("one part")
    ));

    let summary_only = serde_json::json!({
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "just a summary"}]
    });
    assert!(matches!(
        read_reasoning_text(&summary_only),
        std::borrow::Cow::Borrowed("just a summary")
    ));
}

/// Two `content[]` parts + one `summary[]` part must still allocate and concatenate in order —
/// `content[]` fully, then `summary[]` — with NO separator between parts (the existing,
/// deliberately separator-less concat behavior; this test only pins the Cow variant, not a new
/// separator).
#[test]
fn read_reasoning_text_multi_part_concatenates() {
    let item = serde_json::json!({
        "type": "reasoning",
        "content": [
            {"type": "reasoning_text", "text": "first "},
            {"type": "reasoning_text", "text": "second "}
        ],
        "summary": [{"type": "summary_text", "text": "third"}]
    });
    let text = read_reasoning_text(&item);
    assert!(matches!(text, std::borrow::Cow::Owned(_)));
    assert_eq!(text.as_ref(), "first second third");
}

/// Cross-array edge cases: exactly one accepted part in EITHER array alone still borrows; one in
/// each array must allocate and concatenate BOTH (not silently drop one).
#[test]
fn read_reasoning_text_cross_array_borrow_vs_allocate() {
    let content_one_summary_zero = serde_json::json!({
        "type": "reasoning",
        "content": [{"type": "reasoning_text", "text": "C"}],
        "summary": []
    });
    assert!(matches!(
        read_reasoning_text(&content_one_summary_zero),
        std::borrow::Cow::Borrowed("C")
    ));

    let content_zero_summary_one = serde_json::json!({
        "type": "reasoning",
        "content": [],
        "summary": [{"type": "summary_text", "text": "S"}]
    });
    assert!(matches!(
        read_reasoning_text(&content_zero_summary_one),
        std::borrow::Cow::Borrowed("S")
    ));

    let one_in_each = serde_json::json!({
        "type": "reasoning",
        "content": [{"type": "reasoning_text", "text": "C"}],
        "summary": [{"type": "summary_text", "text": "S"}]
    });
    let text = read_reasoning_text(&one_in_each);
    assert!(matches!(text, std::borrow::Cow::Owned(_)));
    assert_eq!(
        text.as_ref(),
        "CS",
        "must concatenate both, not drop either"
    );
}

/// Empty-case discipline: no accepted part in either array must return `Cow::Borrowed("")`, never
/// `Cow::Owned(String::new())` — a caller that only wants a length (`total_text_chars`) must not
/// pay an allocation for zero text.
#[test]
fn read_reasoning_text_empty_case_borrows_not_owns() {
    let no_arrays = serde_json::json!({"type": "reasoning"});
    assert!(matches!(
        read_reasoning_text(&no_arrays),
        std::borrow::Cow::Borrowed("")
    ));

    let empty_arrays = serde_json::json!({"type": "reasoning", "content": [], "summary": []});
    assert!(matches!(
        read_reasoning_text(&empty_arrays),
        std::borrow::Cow::Borrowed("")
    ));
}

#[test]
fn response_blank_call_id_is_synthesized() {
    let json = serde_json::json!({
        "id": "resp_x",
        "object": OBJ_RESPONSE,
        "status": STATUS_COMPLETED,
        "output": [
            {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "", "name": "get_weather", "arguments": "{}"},
            {"type": ITEM_TYPE_FUNCTION_CALL, "name": "get_time", "arguments": "{}"}
        ],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let resp = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response");
    let ids: Vec<&str> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both tool calls must be read: {:?}",
        resp.content
    );
    for id in &ids {
        assert!(!id.is_empty(), "a synthesized call_id must never be blank");
        assert!(
            id.starts_with("call_"),
            "synthesized id must be call_…: {id}"
        );
    }
    assert_ne!(ids[0], ids[1], "distinct tool calls must get distinct ids");
}

#[test]
fn response_object_function_arguments_preserved() {
    let json = serde_json::json!({
        "id": "resp_x",
        "object": OBJ_RESPONSE,
        "status": STATUS_COMPLETED,
        "output": [
            {"type": ITEM_TYPE_FUNCTION_CALL, "call_id": "fc_1", "name": "get_weather",
             "arguments": {"city": "SF", "unit": "c"}}
        ],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let resp = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &json)
        .expect("read_response");
    let input = resp
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::ToolUse { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a ToolUse block");
    assert_eq!(input.get("city").and_then(|v| v.as_str()), Some("SF"));
    assert_eq!(input.get("unit").and_then(|v| v.as_str()), Some("c"));
}

/// The writer's four per-item accumulators (`tool_calls`, `text_accum`, `citation_accum`,
/// `reasoning_accum`) grow one entry per distinct block index a backend streams.
#[test]
fn writer_accumulators_are_bounded_in_entry_count() {
    let writer = ResponsesWriter;
    let citation = crate::codec::ir::IrCitation {
        domain: None,
        kind: Some("web_search_result_location".to_string()),
        cited_text: None,
        title: None,
        url: Some("https://example.invalid/a".to_string()),
        document_index: None,
        start_index: None,
        end_index: None,
        encrypted_index: None,
        raw: None,
        ..Default::default()
    };
    for index in 0..(MAX_OPEN_TOOLS + 50) {
        writer.append_tool_arguments(index, "{}");
        writer.append_text(index, "hi");
        writer.append_citations(index, std::slice::from_ref(&citation));
        writer.append_reasoning(index, "why");
    }

    assert!(
        writer.stream().tool_calls.len() <= MAX_OPEN_TOOLS,
        "tool_calls grew past the cap: {}",
        writer.stream().tool_calls.len()
    );
    assert!(
        writer.stream().text_accum.len() <= MAX_OPEN_TOOLS,
        "text_accum grew past the cap: {}",
        writer.stream().text_accum.len()
    );
    assert!(
        writer.stream().citation_accum.len() <= MAX_OPEN_TOOLS,
        "citation_accum grew past the cap: {}",
        writer.stream().citation_accum.len()
    );
    assert!(
        writer.stream().reasoning_accum.len() <= MAX_OPEN_TOOLS,
        "reasoning_accum grew past the cap: {}",
        writer.stream().reasoning_accum.len()
    );

    assert_eq!(
        writer.stream().text_accum.len(),
        MAX_OPEN_TOOLS,
        "the cap admits exactly MAX_OPEN_TOOLS distinct items"
    );
}

/// A single tool-call `arguments` accumulator must also stop growing at the operator's translate-body
/// cap — one open index fed an unbounded run of fragments was previously unbounded in BYTES.
#[test]
fn writer_tool_arguments_stop_growing_at_the_translate_cap() {
    let cap = crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT;
    let writer = ResponsesWriter;
    writer.record_tool_meta(0, "call_x", "lookup");
    writer.mark_tool_open(0);
    writer.append_tool_arguments(0, "{\"blob\":\"");
    let fragment = "a".repeat(1024 * 1024);
    for _ in 0..((cap / fragment.len()) + 8) {
        writer.append_tool_arguments(0, &fragment);
    }
    {
        let stream = writer.stream();
        let map = &stream.tool_calls;
        let args = &map.get(&0).expect("the open tool accumulates").arguments;
        assert!(
            args.len() <= cap,
            "the arguments accumulator must never exceed the translate cap ({cap}), got {}",
            args.len()
        );
        assert!(
            args.len() + fragment.len() > cap,
            "the test must actually have driven the buffer up TO the cap, got {}",
            args.len()
        );
    }

    let frames = writer.write_response_events(
        crate::codec::test_host::HOST,
        &crate::codec::ir::IrStreamEvent::BlockStop { index: 0 },
    );
    let done = frames
        .iter()
        .find(|(et, _)| et == "response.output_item.done")
        .map(|(_, d)| d)
        .expect("BlockStop on an open tool item emits output_item.done");
    assert_eq!(
        done.pointer("/item/name").and_then(|v| v.as_str()),
        Some("lookup"),
        "the finalized item keeps the call's name: {done}"
    );
    assert!(
        done.pointer("/item/arguments")
            .and_then(|v| v.as_str())
            .is_some(),
        "the finalized item still carries an arguments string: {done}"
    );
}

/// Q128 arch u57: THE LOGPROB ACCUMULATOR IS BOUNDED ON BOTH AXES like every sibling on the writer.
#[test]
fn writer_logprob_accumulator_is_bounded_in_entries_and_bytes() {
    let lp = |token: String| crate::codec::ir::IrTokenLogprob {
        token,
        logprob: -0.1,
        bytes: None,
        bytes_null: false,
        top: Vec::new(),
    };
    let writer = ResponsesWriter;
    for index in 0..(MAX_OPEN_TOOLS + 50) {
        writer.append_logprobs(index, &[lp("a".to_string())]);
    }
    assert_eq!(
        writer.stream().logprob_accum.len(),
        MAX_OPEN_TOOLS,
        "logprob_accum admits exactly MAX_OPEN_TOOLS distinct items"
    );

    let cap = crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT;
    let writer = ResponsesWriter;
    let batch: Vec<_> = (0..64).map(|_| lp("b".repeat(16 * 1024))).collect();
    let weight: usize = batch.iter().map(logprob_bytes).sum();
    for _ in 0..((cap / weight) + 8) {
        writer.append_logprobs(0, &batch);
    }
    let stream = writer.stream();
    let map = &stream.logprob_accum;
    let held: usize = map
        .get(&0)
        .expect("the item accumulates")
        .items
        .iter()
        .map(logprob_bytes)
        .sum();
    assert!(
        held <= cap,
        "logprobs never exceed the translate cap ({cap}), got {held}"
    );
    assert!(
        held + weight > cap,
        "the test drove the buffer up to the cap, got {held}"
    );
    assert_eq!(
        map.get(&0).map(|accum| accum.items.len()).unwrap_or(0) % batch.len(),
        0,
        "a batch is kept whole or dropped whole"
    );
}

/// LLM3#4: THE CITATION ACCUMULATOR IS LINEAR. Each item keeps its held text weight beside its
/// citations, so an append weighs only the batch it brings, never the held list again (which made
/// a long run of citation deltas against one item quadratic), and the weight it keeps is exactly
/// [`citation_bytes`] summed over what it holds, through the cap's refusals.
#[test]
fn writer_citation_accumulator_keeps_its_held_weight() {
    let raw = serde_json::json!({"type": "url_citation", "url": "https://example.invalid/r"});
    let citation = crate::codec::ir::IrCitation {
        kind: Some("web_search_result_location".to_string()),
        cited_text: Some("c".repeat(16 * 1024)),
        url: Some("https://example.invalid/a".to_string()),
        raw: Some(raw.clone()),
        ..Default::default()
    };
    let one = citation_bytes(&citation);
    assert_eq!(
        one,
        std::mem::size_of::<crate::codec::ir::IrCitation>()
            + "web_search_result_location".len()
            + 16 * 1024
            + "https://example.invalid/a".len()
            + raw.to_string().len(),
        "a citation weighs its retained struct, strings, and compact raw entry"
    );

    let cap = crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT;
    let writer = ResponsesWriter;
    let batch: Vec<_> = (0..8).map(|_| citation.clone()).collect();
    let weight = one * batch.len();
    for _ in 0..((cap / weight) + 8) {
        writer.append_citations(0, &batch);
    }
    let stream = writer.stream();
    let map = &stream.citation_accum;
    let entry = map.get(&0).expect("the item accumulates");
    let summed: usize = entry.items.iter().map(citation_bytes).sum();
    assert_eq!(
        entry.bytes, summed,
        "the kept weight is the held list's weight"
    );
    assert!(
        entry.bytes <= cap,
        "citations never exceed the translate cap ({cap})"
    );
    assert!(
        entry.bytes + weight > cap,
        "the test drove the buffer up to the cap, got {}",
        entry.bytes
    );
    assert_eq!(
        entry.items.len() % batch.len(),
        0,
        "a batch is kept whole or dropped whole"
    );
    drop(stream);
    assert_eq!(
        writer.take_citation_accum(0).len(),
        summed / one,
        "take hands back every held citation"
    );
}

/// A URL citation must SURVIVE a Responses→Responses hop.
#[test]
fn responses_url_citation_survives_a_responses_round_trip() {
    let resp = crate::codec::ir::IrResponse {
        logprobs: Vec::new(),
        role: crate::codec::ir::IrRole::Assistant,
        id: Some("resp_rt".to_string()),
        model: Some("gpt-4o".to_string()),
        created: Some(1_700_000_000),
        content: vec![crate::codec::ir::IrBlock::Text {
            text: "See the source for details.".to_string(),
            cache_control: None,
            citations: vec![crate::codec::ir::IrCitation {
                domain: None,
                kind: Some("web_search_result_location".to_string()),
                cited_text: None,
                title: Some("Responses Source".to_string()),
                url: Some("https://example.com/r".to_string()),
                document_index: None,
                start_index: Some(4),
                end_index: Some(10),
                encrypted_index: None,
                raw: None,
                ..Default::default()
            }],
            refusal: false,
        }],
        stop_reason: Some(crate::codec::ir::IrStopReason::EndTurn),
        stop_sequence: None,
        usage: crate::codec::ir::IrUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            detail: crate::codec::ir::IrUsageDetail::default(),
        },
        system_fingerprint: None,
        request_echo: None,
        stop_detail: None,
        ..Default::default()
    };

    let writer = ResponsesWriter;
    let wire = writer.write_response(crate::codec::test_host::HOST, &resp);

    let ann = wire
        .pointer("/output/0/content/0/annotations/0")
        .expect("the output text part carries an annotation");
    assert_eq!(ann["type"], "url_citation");
    assert_eq!(
        ann.get("url").and_then(|v| v.as_str()),
        Some("https://example.com/r"),
        "url is flattened onto the entry: {ann}"
    );
    assert!(
        ann.get("url_citation").is_none(),
        "the Responses wire must NOT nest under url_citation: {ann}"
    );

    let back = ResponsesReader
        .read_response(crate::codec::test_host::HOST, &wire)
        .expect("read_response should succeed");
    let citations = back
        .content
        .iter()
        .find_map(|b| match b {
            crate::codec::ir::IrBlock::Text { citations, .. } => Some(citations),
            _ => None,
        })
        .expect("a Text block must be present");
    assert_eq!(
        citations.len(),
        1,
        "the citation must survive the round-trip, not be dropped: {citations:?}"
    );
    assert_eq!(
        citations[0].url.as_deref(),
        Some("https://example.com/r"),
        "{citations:?}"
    );
    assert_eq!(
        citations[0].title.as_deref(),
        Some("Responses Source"),
        "{citations:?}"
    );
}

#[test]
fn status_word_golden() {
    let golden: [(StatusClass, &str); 9] = [
        (StatusClass::RateLimit, "rate_limit_exceeded"),
        (StatusClass::Overloaded, "server_error"),
        (StatusClass::ServerError, "server_error"),
        (StatusClass::Timeout, "server_error"),
        (StatusClass::Network, "server_error"),
        (StatusClass::Auth, "server_error"),
        (StatusClass::Billing, "server_error"),
        (StatusClass::ClientError, "invalid_prompt"),
        (StatusClass::ContextLength, "invalid_prompt"),
    ];
    for (class, word) in golden {
        let err = CanonicalSignal {
            class,
            provider_signal: None,
            retry_after: None,
        };
        assert_eq!(responses_error_code(&err), word, "class={class:?}");
    }
}

/// A top-level Responses error is terminal, including when the provider omits its code.
#[test]
fn a_top_level_error_closes_open_blocks_and_fails_the_stream() {
    use crate::codec::ir::IrStreamEvent;
    for data in [
        serde_json::json!({"type":"error", "code":"server_error", "message":"boom"}),
        serde_json::json!({"type":"error", "code":null, "message":"boom"}),
        serde_json::json!({"type":"error", "message":"boom"}),
    ] {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        state.open_tools.insert(2);
        let events = reader_read_response_events("error", &data, &mut state);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, IrStreamEvent::Error(_))),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, IrStreamEvent::BlockStop { index: 2 })),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(IrStreamEvent::MessageStop)),
            "{events:?}"
        );
        assert!(state.open_tools.is_empty());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, IrStreamEvent::MessageDelta { .. })),
            "{events:?}"
        );
    }
}

#[test]
fn logprob_weight_survives_refusal_clone_take_and_reset() {
    let lp = crate::codec::ir::IrTokenLogprob {
        token: "token".into(),
        logprob: -0.1,
        bytes: Some(vec![1, 2]),
        bytes_null: false,
        top: vec![crate::codec::ir::IrTopLogprob {
            token: "alternative".into(),
            logprob: -0.2,
            bytes: Some(vec![3, 4, 5]),
            bytes_null: false,
        }],
    };
    let weight = logprob_bytes(&lp);
    let writer = ResponsesWriter;
    writer.store_translate_cap(weight * 2);
    assert!(writer.append_logprobs(0, std::slice::from_ref(&lp)));
    let cloned = writer.clone();
    assert!(writer.append_logprobs(0, std::slice::from_ref(&lp)));
    assert!(!writer.append_logprobs(0, std::slice::from_ref(&lp)));
    {
        let stream = writer.stream();
        let entry = stream.logprob_accum.get(&0).unwrap();
        assert_eq!(entry.bytes, weight * 2);
        assert_eq!(entry.items.len(), 2);
        assert_eq!(
            entry.bytes,
            entry.items.iter().map(logprob_bytes).sum::<usize>()
        );
    }
    assert_eq!(cloned.take_logprob_accum(0), vec![lp.clone()]);
    assert!(cloned.take_logprob_accum(0).is_empty());
    assert!(cloned.append_logprobs(0, &[lp]));
    assert_eq!(cloned.stream().logprob_accum.get(&0).unwrap().bytes, weight);
    writer.reset_sequence_number();
    assert!(writer.stream().logprob_accum.is_empty());
    assert_eq!(cloned.stream().logprob_accum.get(&0).unwrap().bytes, weight);
}

#[test]
fn item_identity_entropy_callback_can_reenter_and_first_insert_wins() {
    use crate::codec::cells::CodecHost;
    struct Fixed;
    impl CodecHost for Fixed {
        fn fill_entropy(&self, out: &mut [u8]) -> bool {
            out.fill(7);
            true
        }
        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }
    struct Reentrant<'a>(&'a ResponsesWriter, bool);
    impl CodecHost for Reentrant<'_> {
        fn fill_entropy(&self, out: &mut [u8]) -> bool {
            let nested = self.0.item_id_for(&Fixed, ITEM_ID_PREFIX_MSG, 0).unwrap();
            assert_eq!(nested, format!("msg_{}", "7".repeat(ITEM_ID_TOKEN_LEN)));
            out.fill(2);
            self.1
        }
        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }
    for outer_draw_succeeds in [true, false] {
        let writer = ResponsesWriter;
        let id = writer
            .item_id_for(
                &Reentrant(&writer, outer_draw_succeeds),
                ITEM_ID_PREFIX_MSG,
                0,
            )
            .unwrap();
        assert_eq!(id, format!("msg_{}", "7".repeat(ITEM_ID_TOKEN_LEN)));
        assert_eq!(
            writer.item_id_for(&Fixed, ITEM_ID_PREFIX_MSG, 0).unwrap(),
            id
        );
        assert_eq!(writer.stream().item_ids.len(), 1);
    }
}

#[test]
fn failed_item_entropy_is_not_cached_or_substituted() {
    use crate::codec::cells::CodecHost;
    struct Failed;
    impl CodecHost for Failed {
        fn fill_entropy(&self, out: &mut [u8]) -> bool {
            out.fill(3);
            false
        }
        fn wall_clock_now(&self) -> Option<u64> {
            None
        }
    }
    let writer = ResponsesWriter;
    assert!(writer.item_id_for(&Failed, ITEM_ID_PREFIX_MSG, 0).is_err());
    assert!(writer.stream().item_ids.is_empty());
    let id = writer
        .item_id_for(crate::codec::test_host::HOST, ITEM_ID_PREFIX_MSG, 0)
        .unwrap();
    assert_eq!(
        writer.item_id_for(&Failed, ITEM_ID_PREFIX_MSG, 0).unwrap(),
        id
    );
}
