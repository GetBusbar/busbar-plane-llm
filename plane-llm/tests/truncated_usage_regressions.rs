//! A native whole response cut by its transport bills only structurally proven root usage.

use busbar_contract::codec::CodecHost;
use busbar_contract::operation::OpVerb;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Piece, Reply, ReplyCtx};
use busbar_plane_llm::exchange::shaping::Lane;
use serde_json::{json, Value};

struct Host;
impl CodecHost for Host {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }
    fn wall_clock_now(&self) -> Option<u64> {
        Some(1_700_000_000)
    }
}

fn cut(protocol: &'static str, body: &[u8], chunk: usize, cap: usize) -> Piece<'static> {
    let request = json!({"model":"model", "stream":false});
    let arrived = Arrived {
        dialect: protocol,
        operation: OpVerb::CHAT,
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
        dialect: protocol,
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
        intent: StreamIntent::default(),
        passthrough: false,
        translate_cap: cap,
    };
    let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"application/json")]);
    for bytes in body.chunks(chunk) {
        let piece = reply.feed(&Host, &ctx, bytes, false, At::default());
        assert!(!piece.done);
    }
    reply.cut(&Host, &ctx, true)
}

fn usage(protocol: &str) -> (&'static str, Value) {
    match protocol {
        "openai" => ("usage", json!({"prompt_tokens":9,"completion_tokens":3})),
        "cohere" => (
            "usage",
            json!({"tokens":{"input_tokens":9,"output_tokens":3}}),
        ),
        "bedrock" => ("usage", json!({"inputTokens":9,"outputTokens":3})),
        "gemini" => (
            "usageMetadata",
            json!({"promptTokenCount":9,"candidatesTokenCount":3}),
        ),
        _ => ("usage", json!({"input_tokens":9,"output_tokens":3})),
    }
}

#[test]
fn every_dialect_refuses_nested_usage_on_the_actual_transport_cut_path() {
    for protocol in [
        "anthropic",
        "openai",
        "responses",
        "cohere",
        "bedrock",
        "gemini",
    ] {
        let (key, counts) = usage(protocol);
        let body = format!("{{\"metadata\":{{\"{key}\":{counts}}},\"after\":");
        for chunk in [1, 7, body.len()] {
            let piece = cut(protocol, body.as_bytes(), chunk, 4096);
            assert_eq!(
                (piece.units.tokens_in, piece.units.tokens_out),
                (0, 0),
                "{protocol}"
            );
            assert!(piece.units.open.is_empty(), "{protocol}");
        }
    }
}

#[test]
fn an_anthropic_tool_input_is_never_a_usage_report() {
    let mut body = serde_json::to_vec(&json!({
        "id":"msg_native", "type":"message", "role":"assistant", "model":"model",
        "content":[{"type":"tool_use", "id":"toolu_native", "name":"inspect",
            "input":{"usage":{"input_tokens":100000000,"output_tokens":100000000}}}]
    }))
    .unwrap();
    body.pop(); // The transport cut before the root closed; no provider usage was received.
    let piece = cut("anthropic", &body, 7, 4096);
    assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (0, 0));
    assert!(piece.units.open.is_empty());
}

#[test]
fn every_dialect_keeps_real_root_usage_beside_nested_decoys_and_cut_metadata() {
    for protocol in [
        "anthropic",
        "openai",
        "responses",
        "cohere",
        "bedrock",
        "gemini",
    ] {
        let (key, counts) = usage(protocol);
        let decoy = json!({"input_tokens":999,"output_tokens":999,"prompt_tokens":999,
            "completion_tokens":999,"inputTokens":999,"outputTokens":999,
            "promptTokenCount":999,"candidatesTokenCount":999});
        let body = format!("{{\"{key}\":{counts},\"metadata\":{{\"{key}\":{decoy}}},\"after\":");
        for chunk in [1, 7, body.len()] {
            let piece = cut(protocol, body.as_bytes(), chunk, 4096);
            assert_eq!(
                (piece.units.tokens_in, piece.units.tokens_out),
                (9, 3),
                "{protocol}"
            );
        }
        let over_cap = cut(protocol, body.as_bytes(), 7, 8);
        assert_eq!(
            (over_cap.units.tokens_in, over_cap.units.tokens_out),
            (0, 0),
            "{protocol}"
        );
    }
}
