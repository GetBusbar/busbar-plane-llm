//! Responses fixes through the production request and reply seams. Execution is a separate check.

mod common;

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::{build, StreamIntent};
use busbar_plane_llm::exchange::reply::{At, Reply, ReplyCtx};
use busbar_plane_llm::exchange::shaping::{Lane, Shaping};
use common::SevensHost;
use serde_json::{json, Value};

fn arrived(dialect: &'static str, request: Value) -> Arrived {
    Arrived {
        dialect,
        operation: OpVerb::CHAT,
        model: "model".into(),
        content_type: "application/json".into(),
        body: serde_json::to_vec(&request).unwrap(),
        parsed: Some(request),
        path: "/v1/responses".into(),
        query: None,
        path_model: None,
    }
}

fn lane(dialect: &'static str) -> Lane {
    Lane {
        model: "model".into(),
        provider: "provider".into(),
        dialect,
        path: None,
        path_base: None,
        options: Vec::new(),
        upstream_model: None,
        default_max_tokens: None,
        context_max: None,
        reasoning: false,
        prompt_caching: false,
        caps: Default::default(),
        error_map: Default::default(),
        statics: &[],
        invoke: false,
    }
}

struct Observed {
    body: Vec<u8>,
    dropped: Vec<String>,
    units: (u64, u64),
    synthesized: bool,
}

fn reply(
    ingress: &'static str,
    egress: &'static str,
    wants_stream: bool,
    far_stream: bool,
    body: &[u8],
    chunk: usize,
) -> Observed {
    let arrived = arrived(ingress, json!({"model":"model", "stream":wants_stream}));
    let lane = lane(egress);
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: StreamIntent {
            wants_stream,
            client_include_usage: true,
        },
        passthrough: false,
        translate_cap: 1024 * 1024,
    };
    let content_type: &[u8] = if far_stream {
        b"text/event-stream"
    } else {
        b"application/json"
    };
    let mut reply = Reply::new(&SevensHost, &ctx, 200, &[(b"content-type", content_type)]);
    let mut observed = Observed {
        body: Vec::new(),
        dropped: Vec::new(),
        units: (0, 0),
        synthesized: false,
    };
    let chunks = body.chunks(chunk);
    let count = chunks.len();
    for (index, bytes) in chunks.enumerate() {
        let piece = reply.feed(&SevensHost, &ctx, bytes, index + 1 == count, At::default());
        observed.body.extend_from_slice(&piece.bytes);
        observed.dropped.extend(piece.dropped);
        observed.units = (piece.units.tokens_in, piece.units.tokens_out);
        observed.synthesized |= piece.synthesized;
        if index + 1 == count {
            assert!(piece.done);
        }
    }
    observed
}

fn frames(body: &[u8]) -> Vec<Value> {
    std::str::from_utf8(body)
        .unwrap()
        .split("\n\n")
        .filter_map(|frame| {
            let (_, data) = busbar_plane_llm::codec::dialect::parse_sse_frame(frame.as_bytes())?;
            serde_json::from_str(&data).ok()
        })
        .collect()
}

fn sse(events: &[(&str, Value)]) -> Vec<u8> {
    events
        .iter()
        .map(|(name, body)| format!("event: {name}\ndata: {body}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn gemini_media() -> Value {
    json!({"responseId":"native-response", "modelVersion":"model",
        "candidates":[{"content":{"role":"model", "parts":[
            {"text":"answer"},
            {"inlineData":{"mimeType":"audio/wav", "data":"UklGRg=="}},
            {"inlineData":{"mimeType":"image/png", "data":"iVBORw=="}}
        ]}, "finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":9, "candidatesTokenCount":3, "totalTokenCount":12}})
}

#[test]
fn generated_media_loss_is_named_on_buffered_synthesized_and_live_responses() {
    let native = gemini_media();
    let whole = serde_json::to_vec(&native).unwrap();
    let live = sse(&[("message", native)]);
    for (wants_stream, far_stream, body) in [
        (false, false, whole.as_slice()),
        (true, false, whole.as_slice()),
        (true, true, live.as_slice()),
    ] {
        for chunk in [1, body.len()] {
            let observed = reply("responses", "gemini", wants_stream, far_stream, body, chunk);
            assert_eq!(observed.units, (9, 3));
            assert_eq!(observed.synthesized, wants_stream && !far_stream);
            assert!(
                observed
                    .dropped
                    .iter()
                    .any(|p| p == "candidates[].content.parts[]"),
                "{:?}",
                observed.dropped
            );
            assert!(!observed
                .dropped
                .iter()
                .any(|p| p.contains("UklGRg") || p.contains("iVBORw")));
            if wants_stream {
                let frames = frames(&observed.body);
                let terminal = frames
                    .iter()
                    .find(|f| f["type"] == "response.completed")
                    .expect("completed");
                assert_eq!(
                    terminal["response"]["output"][0]["content"][0]["text"],
                    "answer"
                );
                assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 1);
                let indices: Vec<_> = frames
                    .iter()
                    .filter(|f| f["type"] == "response.output_item.added")
                    .map(|f| f["output_index"].clone())
                    .collect();
                assert_eq!(indices, vec![json!(0)]);
                assert_eq!(terminal["response"]["usage"]["output_tokens"], 3);
            } else {
                let response: Value = serde_json::from_slice(&observed.body).unwrap();
                assert_eq!(response["output"][0]["content"][0]["text"], "answer");
                assert_eq!(response["output"].as_array().unwrap().len(), 1);
                assert_eq!(response["usage"]["output_tokens"], 3);
            }
        }
    }
}

#[test]
fn native_gemini_history_keeps_flat_item_encounter_order_on_responses_requests() {
    let call = json!({"functionCall":{"id":"native-call", "name":"lookup", "args":{"q":"value"}}});
    let cases = [
        (
            json!([call.clone(), {"text":"after"}]),
            vec!["function_call", "message"],
        ),
        (
            json!([{"text":"before"}, call.clone(), {"text":"after"}]),
            vec!["message", "function_call", "message"],
        ),
        (
            json!([{"text":"before-a"}, {"text":"before-b"}, call.clone(), {"text":"after"}]),
            vec!["message", "function_call", "message"],
        ),
        (
            json!([{"text":"before"}, {"text":"thought", "thought":true}, call, {"text":"after"}]),
            vec!["message", "reasoning", "function_call", "message"],
        ),
    ];
    let shaping = Shaping::from_settings(&json!({
        "providers":{"provider":{"protocol":"responses", "base_url":"https://example.invalid"}},
        "models":{"model":{"provider":"provider", "reasoning":true}},
        "pools":{"pool":{"members":["model"]}}
    }))
    .unwrap();
    for (parts, expected) in cases {
        let request = json!({"contents":[{"role":"model", "parts":parts}]});
        let arrived = arrived("gemini", request);
        let far = build(
            &SevensHost,
            &arrived,
            &[(b"content-type", b"application/json")],
            &shaping,
            "pool",
            "model",
        )
        .unwrap();
        let output: Value = serde_json::from_slice(&far.body).unwrap();
        let input = output["input"].as_array().unwrap();
        let kinds: Vec<_> = input
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("message"))
            .collect();
        assert_eq!(kinds, expected, "{output}");
        let call = input
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["name"], "lookup");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            json!({"q":"value"})
        );
        assert_eq!(input.last().unwrap()["content"][0]["text"], "after");
        if input[0]["content"][0]["text"] == "before-a" {
            assert_eq!(input[0]["content"].as_array().unwrap().len(), 2);
            assert_eq!(input[0]["content"][1]["text"], "before-b");
        }
        for item in input.iter().filter(|i| i.get("role").is_some()) {
            assert_eq!(item["role"], "assistant");
            assert!(!item["content"].as_array().unwrap().is_empty());
        }
    }
}

fn file_annotations() -> Value {
    json!([
        {"type":"file_citation", "file_id":"file_1", "filename":"a.pdf", "index":4},
        {"type":"container_file_citation", "container_id":"container_1", "file_id":"file_2", "filename":"b.csv", "start_index":0, "end_index":8},
        {"type":"file_path", "file_id":"file_3", "index":9}
    ])
}

fn responses_body(annotations: Value) -> Value {
    json!({"id":"resp_native", "object":"response", "created_at":1700000000,
        "model":"model", "status":"completed", "output":[{
            "type":"message", "id":"msg_native", "status":"completed", "role":"assistant",
            "content":[{"type":"output_text", "text":"see file", "logprobs":[], "annotations":annotations}]
        }], "usage":{"input_tokens":9, "output_tokens":3, "total_tokens":12}})
}

#[test]
fn native_file_annotations_survive_synthesis_on_every_finalized_surface() {
    let annotations = file_annotations();
    let far = serde_json::to_vec(&responses_body(annotations.clone())).unwrap();
    for chunk in [1, far.len()] {
        let observed = reply("responses", "responses", true, false, &far, chunk);
        assert!(observed.synthesized);
        assert_eq!(observed.units, (9, 3));
        let frames = frames(&observed.body);
        for (kind, pointer) in [
            ("response.content_part.done", "/part/annotations"),
            ("response.output_item.done", "/item/content/0/annotations"),
            (
                "response.completed",
                "/response/output/0/content/0/annotations",
            ),
        ] {
            let frame = frames.iter().find(|f| f["type"] == kind).expect(kind);
            assert_eq!(
                frame.pointer(pointer),
                Some(&annotations),
                "{kind}: {frame}"
            );
        }
        let ids: Vec<_> = frames.iter().filter_map(|f| f.get("item_id")).collect();
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| *id == ids[0]));
        let sequence: Vec<_> = frames
            .iter()
            .filter_map(|f| f["sequence_number"].as_u64())
            .collect();
        assert_eq!(sequence, (0..frames.len() as u64).collect::<Vec<_>>());
    }
}

#[test]
fn url_only_responses_citations_are_audited_on_anthropic_with_text_and_usage_kept() {
    let annotations = json!([{"type":"url_citation", "url":"https://example.invalid/source", "title":"source", "start_index":0, "end_index":8}]);
    let response = responses_body(annotations.clone());
    let whole = serde_json::to_vec(&response).unwrap();
    let live = sse(&[
        (
            "response.created",
            json!({"response":{"id":"resp_native", "created_at":1700000000, "model":"model"}}),
        ),
        (
            "response.output_item.added",
            json!({"output_index":0, "item":{"type":"message", "role":"assistant", "content":[]}}),
        ),
        (
            "response.output_text.delta",
            json!({"output_index":0, "delta":"see file"}),
        ),
        (
            "response.output_text.annotation.added",
            json!({"output_index":0, "content_index":0, "annotation_index":0, "annotation":annotations[0]}),
        ),
        (
            "response.output_item.done",
            json!({"output_index":0, "item":response["output"][0]}),
        ),
        ("response.completed", json!({"response":response})),
    ]);
    for (wants_stream, far_stream, body) in [
        (false, false, whole.as_slice()),
        (true, false, whole.as_slice()),
        (true, true, live.as_slice()),
    ] {
        let observed = reply("anthropic", "responses", wants_stream, far_stream, body, 7);
        assert_eq!(observed.units, (9, 3));
        assert!(
            observed.dropped.iter().any(|p| p.contains("citations")),
            "{:?}",
            observed.dropped
        );
        if wants_stream {
            let frames = frames(&observed.body);
            let text: String = frames
                .iter()
                .filter_map(|f| f.pointer("/delta/text").and_then(Value::as_str))
                .collect();
            assert_eq!(text, "see file");
            assert!(!frames
                .iter()
                .any(|f| f.pointer("/delta/type") == Some(&json!("citations_delta"))));
        } else {
            let response: Value = serde_json::from_slice(&observed.body).unwrap();
            assert_eq!(response["content"][0]["text"], "see file");
            assert!(response["content"][0]
                .get("citations")
                .is_none_or(|c| c.is_null() || c.as_array().is_some_and(Vec::is_empty)));
        }
        assert!(!String::from_utf8_lossy(&observed.body).contains("encrypted_index"));
    }
}

#[test]
fn chat_logprob_batches_keep_order_and_finalize_every_responses_surface() {
    let tokens = ["one", " two", " three"];
    let mut events = Vec::new();
    for token in tokens {
        events.push(("message", json!({
            "id":"chatcmpl_native", "object":"chat.completion.chunk", "created":1700000000, "model":"model",
            "choices":[{"index":0, "delta":{"role":"assistant", "content":token}, "finish_reason":null,
                "logprobs":{"content":[{"token":token, "logprob":-0.1, "bytes":token.as_bytes(),
                    "top_logprobs":[{"token":"alternative", "logprob":-0.2, "bytes":"alternative".as_bytes()}]}]}}]
        })));
    }
    events.push((
        "message",
        json!({
            "id":"chatcmpl_native", "object":"chat.completion.chunk", "model":"model",
            "choices":[{"index":0, "delta":{}, "finish_reason":"stop"}],
            "usage":{"prompt_tokens":9, "completion_tokens":3, "total_tokens":12}
        }),
    ));
    let body = sse(&events);
    for chunk in [1, body.len()] {
        let observed = reply("responses", "openai", true, true, &body, chunk);
        assert_eq!(observed.units, (9, 3));
        let frames = frames(&observed.body);
        for (kind, pointer) in [
            ("response.output_text.done", "/logprobs"),
            ("response.content_part.done", "/part/logprobs"),
            ("response.output_item.done", "/item/content/0/logprobs"),
            (
                "response.completed",
                "/response/output/0/content/0/logprobs",
            ),
        ] {
            let frame = frames.iter().find(|f| f["type"] == kind).expect(kind);
            let entries = frame.pointer(pointer).unwrap().as_array().unwrap();
            let actual: Vec<_> = entries
                .iter()
                .map(|lp| lp["token"].as_str().unwrap())
                .collect();
            assert_eq!(actual, tokens, "{kind}: {frame}");
            assert!(entries
                .iter()
                .all(|lp| lp["top_logprobs"][0]["token"] == "alternative"));
        }
    }
}

#[test]
fn native_anthropic_tool_result_keeps_its_position_between_user_text() {
    let request = json!({"model":"model", "max_tokens":64, "messages":[
        {"role":"assistant", "content":[{"type":"tool_use", "id":"toolu_native", "name":"lookup", "input":{}}]},
        {"role":"user", "content":[
            {"type":"text", "text":"before"},
            {"type":"tool_result", "tool_use_id":"toolu_native", "content":"result"},
            {"type":"text", "text":"after"}
        ]}
    ]});
    let shaping = Shaping::from_settings(&json!({
        "providers":{"provider":{"protocol":"responses", "base_url":"https://example.invalid"}},
        "models":{"model":{"provider":"provider"}},
        "pools":{"pool":{"members":["model"]}}
    }))
    .unwrap();
    let arrived = arrived("anthropic", request);
    let far = build(
        &SevensHost,
        &arrived,
        &[(b"content-type", b"application/json")],
        &shaping,
        "pool",
        "model",
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&far.body).unwrap();
    let input = output["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{output}");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[1]["content"][0]["text"], "before");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], input[0]["call_id"]);
    assert_eq!(input[2]["output"], "result");
    assert_eq!(input[3]["content"][0]["text"], "after");
}
