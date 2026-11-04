//! Provider projection and refusal accounting through the production Reply boundary.

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::codec::cells::CodecHost;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Piece, Reply, ReplyCtx};
use busbar_plane_llm::exchange::shaping::Lane;
use serde_json::{json, Value};

struct Host(bool);
impl CodecHost for Host {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        self.0
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1)
    }
}

fn answer<'a>(
    host: &dyn CodecHost,
    ingress: &'static str,
    egress: &'static str,
    operation: OpVerb,
    stream: bool,
    upstream_stream: bool,
    body: &'a [u8],
) -> Piece<'a> {
    let request = json!({"model":"model","stream":stream});
    let arrived = Arrived {
        dialect: ingress,
        operation,
        model: "model".into(),
        content_type: "application/json".into(),
        body: serde_json::to_vec(&request).unwrap(),
        parsed: Some(request),
        path: "/v1/chat/completions".into(),
        query: None,
        path_model: None,
    };
    let lane = Lane {
        model: "model".into(),
        provider: "provider".into(),
        dialect: egress,
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
    };
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: StreamIntent {
            wants_stream: stream,
            client_include_usage: false,
        },
        passthrough: false,
        translate_cap: 4096,
    };
    let content_type: &[u8] = if upstream_stream {
        b"text/event-stream"
    } else {
        b"application/json"
    };
    Reply::new(host, &ctx, 200, &[(b"content-type", content_type)]).feed(
        host,
        &ctx,
        body,
        true,
        At {
            now_s: 1,
            elapsed_ms: None,
        },
    )
}

fn sse(frames: &[Value]) -> Vec<u8> {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn cohere_bounds_same_dialect_answer_keeps_native_bytes_and_usage() {
    let body = br#"{ "id" : "cohere-native", "finish_reason" : "COMPLETE", "message" : { "role" : "assistant", "content" : [{ "type" : "text", "text" : "hi" }] }, "usage" : { "tokens" : { "input_tokens" : 11, "output_tokens" : 13 }, "billed_units" : { "input_tokens" : 7, "output_tokens" : 9 } } }"#;
    let piece = answer(
        &Host(true),
        "cohere",
        "cohere",
        OpVerb::CHAT,
        false,
        false,
        body,
    );
    assert_eq!(piece.bytes.as_ref(), body);
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (11, 13));
    assert!(piece.dropped.is_empty(), "{piece:?}");
    assert!(piece.fault.is_none(), "{piece:?}");
}

#[test]
fn malformed_streamed_arguments_refuse_at_normal_stop_and_transport_eof_with_usage() {
    for finish in [Some("tool_calls"), Some("length"), None] {
        for arguments in ["{\"recipient\":", "{bad}"] {
            let mut frames = vec![json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"call-native","type":"function","function":{"name":"send","arguments":arguments}}]}}]})];
            frames.push(match finish {
                Some(reason) => json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}],
                    "usage":{"prompt_tokens":7,"completion_tokens":9,"total_tokens":16}}),
                None => json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":9,"total_tokens":16}}),
            });
            let body = sse(&frames);
            let piece = answer(
                &Host(true),
                "gemini",
                "openai",
                OpVerb::CHAT,
                true,
                true,
                &body,
            );
            let output = String::from_utf8(piece.bytes.to_vec()).unwrap();
            assert!(output.contains("INTERNAL"), "{finish:?}: {output}");
            assert!(!output.contains("functionCall"), "{finish:?}: {output}");
            assert!(!output.contains("finishReason"), "{finish:?}: {output}");
            assert_eq!(
                (piece.units.tokens_in, piece.units.tokens_out),
                (7, 9),
                "{finish:?}"
            );
            assert!(
                piece.fault.is_some(),
                "encoding refusal is observable: {piece:?}"
            );
        }
    }
}

#[test]
fn malformed_whole_arguments_cannot_become_an_empty_call_in_a_synthesized_stream() {
    let body = serde_json::to_vec(&json!({"id":"chatcmpl-native","model":"m",
        "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[
            {"id":"call-native","type":"function","function":{"name":"send","arguments":"{\"recipient\":"}}]},
            "finish_reason":"length"}],"usage":{"prompt_tokens":7,"completion_tokens":9}})).unwrap();
    let piece = answer(
        &Host(true),
        "gemini",
        "openai",
        OpVerb::CHAT,
        true,
        false,
        &body,
    );
    let output = String::from_utf8(piece.bytes.to_vec()).unwrap();
    assert!(output.contains("INTERNAL"), "{output}");
    assert!(!output.contains("functionCall"), "{output}");
    assert!(!output.contains("finishReason"), "{output}");
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (7, 9));
}

#[test]
fn malformed_openai_tool_arguments_refuse_anthropic_without_billing_the_abort() {
    let body = serde_json::to_vec(&json!({"id":"chatcmpl-native", "model":"m",
        "choices":[{"index":0, "message":{"role":"assistant", "content":null,
            "tool_calls":[{"id":"call-native", "type":"function", "function":{
                "name":"send", "arguments":"not json"}}]}, "finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":50, "completion_tokens":20, "total_tokens":70}}))
    .unwrap();
    let piece = answer(
        &Host(true),
        "anthropic",
        "openai",
        OpVerb::CHAT,
        false,
        false,
        &body,
    );
    assert!(
        piece.head.as_ref().is_some_and(|head| head.status >= 500),
        "{piece:?}"
    );
    let refused: Value = serde_json::from_slice(&piece.bytes).unwrap();
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(refused["error"]["type"], "api_error", "{refused}");
    assert_eq!(
        refused["error"]["message"],
        "An internal error occurred while processing the response."
    );
    assert!(refused.get("content").is_none(), "{refused}");
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (0, 0));
}

#[test]
fn refused_embedding_vectors_are_unbilled_and_do_not_compact_coordinates() {
    for embedding in [
        json!({}),
        json!({"values":null}),
        json!({"values":[0.1,null,0.3]}),
        json!({"values":[0.1,"0.2",0.3]}),
        json!({"values":[0.1,1e100,0.3]}),
    ] {
        let body = serde_json::to_vec(
            &json!({"embedding":embedding,"usageMetadata":{"promptTokenCount":5}}),
        )
        .unwrap();
        let piece = answer(
            &Host(true),
            "openai",
            "gemini",
            OpVerb::EMBEDDINGS,
            false,
            false,
            &body,
        );
        assert!(
            piece.head.as_ref().is_some_and(|head| head.status >= 500),
            "{piece:?}"
        );
        assert_eq!(
            (piece.units.tokens_in, piece.units.tokens_out),
            (0, 0),
            "{piece:?}"
        );
        let output: Value = serde_json::from_slice(&piece.bytes).unwrap();
        assert!(output.get("error").is_some(), "{output}");
        assert!(
            output.get("data").is_none(),
            "no shortened vector is delivered: {output}"
        );
    }
    let body = serde_json::to_vec(&json!({"embedding":{"values":[0.1,0.2,0.3]},
        "usageMetadata":{"promptTokenCount":5}}))
    .unwrap();
    let piece = answer(
        &Host(true),
        "openai",
        "gemini",
        OpVerb::EMBEDDINGS,
        false,
        false,
        &body,
    );
    let output: Value = serde_json::from_slice(&piece.bytes).unwrap();
    assert_eq!(
        output["data"][0]["embedding"].as_array().unwrap().len(),
        3,
        "{output}"
    );
    assert_eq!(piece.units.tokens_in, 5);
}

#[test]
fn failed_entropy_reply_refuses_without_inventing_identity_or_billing_the_abort() {
    let body = serde_json::to_vec(&json!({"id":"chatcmpl-native","model":"m","created":1,
        "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":9}})).unwrap();
    let piece = answer(
        &Host(false),
        "gemini",
        "openai",
        OpVerb::CHAT,
        false,
        false,
        &body,
    );
    let output: Value = serde_json::from_slice(&piece.bytes).unwrap();
    assert!(
        piece.head.as_ref().is_some_and(|head| head.status >= 500),
        "{piece:?}"
    );
    assert!(output.get("error").is_some(), "{output}");
    assert!(
        output.get("responseId").is_none() && output.get("candidates").is_none(),
        "{output}"
    );
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (0, 0));
}

#[test]
fn reflected_gemini_bad_key_prose_remains_client_error_on_the_actual_failure_route() {
    use busbar_contract::upstream::{Disposition, StatusClass};
    use busbar_plane_llm::exchange::reply::failure::{judge, FarError};
    let default_map = std::collections::HashMap::new();
    let auth_map = std::collections::HashMap::from([("auth".to_string(), "auth".to_string())]);
    for phrase in [
        "api key not valid",
        "invalid api key",
        "api key has expired",
    ] {
        let body = serde_json::to_vec(&json!({"error":{"code":400,"status":"INVALID_ARGUMENT",
            "message":format!("Invalid JSON payload received. Unknown name \"{phrase}\" at 'contents[0]': Cannot find field.")}})).unwrap();
        for error_map in [&default_map, &auth_map] {
            let judged = judge(
                &Host(true),
                "gemini",
                "gemini",
                OpVerb::CHAT,
                error_map,
                false,
                &FarError {
                    status: 400,
                    head: &[],
                    body: &body,
                },
            );
            assert_eq!(judged.signal.unwrap().class, StatusClass::ClientError);
            assert_eq!(judged.disposition, Disposition::ClientFault);
            assert_eq!(judged.answer.status, 400);
            assert_eq!(
                judged.answer.body, body,
                "native validation bytes remain intact"
            );
        }
    }
    for error in [
        json!({"code":400,"status":"INVALID_ARGUMENT","message":"API key not valid. Please pass a valid API key."}),
        json!({"code":400,"status":"INVALID_ARGUMENT","message":"API key expired. Please renew the API key."}),
        json!({"code":400,"status":"INVALID_ARGUMENT","message":"Permission denied.","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}),
    ] {
        let body = serde_json::to_vec(&json!({"error":error})).unwrap();
        let far = FarError {
            status: 400,
            head: &[],
            body: &body,
        };
        let default = judge(
            &Host(true),
            "gemini",
            "gemini",
            OpVerb::CHAT,
            &default_map,
            false,
            &far,
        );
        assert_eq!(
            default.signal.unwrap().class,
            StatusClass::ClientError,
            "the actual default normalizer uses the received HTTP status"
        );
        let configured = judge(
            &Host(true),
            "gemini",
            "gemini",
            OpVerb::CHAT,
            &auth_map,
            false,
            &far,
        );
        assert_eq!(configured.signal.unwrap().class, StatusClass::Auth);
        assert_eq!(configured.disposition, Disposition::HardDown);
    }
}
