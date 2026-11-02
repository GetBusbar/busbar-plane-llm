//! Request format custody through the multipart arrival, actual attempt and whole reply.

use busbar_contract::codec::CodecHost;
use busbar_plane_llm::exchange::{
    arrive::arrive,
    attempt::{build, StreamIntent},
    reply::{At, Reply, ReplyCtx},
    shaping::Shaping,
};
use serde_json::json;

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

const CONTENT_TYPE: &[u8] = b"multipart/form-data; boundary=fixture";

fn multipart(format: &str) -> Vec<u8> {
    format!("--fixture\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmodel\r\n\
        --fixture\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n{format}\r\n\
        --fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFFfixture\r\n\
        --fixture--\r\n").into_bytes()
}

fn shaping(protocol: &str) -> Shaping {
    Shaping::from_settings(&json!({"providers":{"provider":{"protocol":protocol,"base_url":"https://provider.invalid"}},
        "models":{"model":{"provider":"provider"}}})).unwrap()
}

#[test]
fn a_native_text_format_survives_a_gemini_transcription_reply() {
    let shaping = shaping("gemini");
    let caller: &[(&[u8], &[u8])] = &[(b"content-type", CONTENT_TYPE)];
    for text in [
        "hello",
        "42",
        "true",
        "null",
        "\"hello\"",
        " {\"text\":\"literal\"} ",
    ] {
        let body = multipart("text");
        let arrived = arrive("POST", "/v1/audio/transcriptions", caller, &body, &shaping).unwrap();
        assert!(
            arrived.parsed.is_none(),
            "the native request is multipart, not a JSON echo"
        );
        let attempt = build(&Host, &arrived, caller, &shaping, "", "model").unwrap();
        assert!(!attempt.body.is_empty());
        let ctx = ReplyCtx {
            arrived: &arrived,
            lane: shaping.lane("model").unwrap(),
            intent: StreamIntent::default(),
            passthrough: false,
            translate_cap: 4096,
        };
        let far = serde_json::to_vec(
            &json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]}}],
            "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}}),
        )
        .unwrap();
        let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"application/json")]);
        let piece = reply.feed(&Host, &ctx, &far, true, At::default());
        let head = piece.head.unwrap();
        assert_eq!(head.status, 200);
        assert!(head
            .fields
            .iter()
            .any(|(name, value)| name == "content-type" && value == b"text/plain"));
        assert_eq!(piece.bytes.as_ref(), text.as_bytes());
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (9, 3));
    }
}

#[test]
fn timestamped_formats_are_refused_before_a_foreign_transcription_attempt() {
    let shaping = shaping("gemini");
    let caller: &[(&[u8], &[u8])] = &[(b"content-type", CONTENT_TYPE)];
    for format in ["srt", "vtt"] {
        let body = multipart(format);
        let arrived = arrive("POST", "/v1/audio/transcriptions", caller, &body, &shaping).unwrap();
        let refused = build(&Host, &arrived, caller, &shaping, "", "model").unwrap_err();
        assert_eq!(refused.status, 400);
        assert!(refused
            .message
            .contains("timestamped output is unavailable"));
    }
}

#[test]
fn native_timestamped_requests_relay_without_the_foreign_format_guard() {
    let shaping = shaping("openai");
    let caller: &[(&[u8], &[u8])] = &[(b"content-type", CONTENT_TYPE)];
    for format in ["srt", "vtt"] {
        let body = multipart(format);
        let arrived = arrive("POST", "/v1/audio/transcriptions", caller, &body, &shaping).unwrap();
        let attempt = build(&Host, &arrived, caller, &shaping, "", "model").unwrap();
        assert_eq!(attempt.body, body);
    }
}

#[test]
fn format_echo_uses_the_same_bounded_named_fields_as_transcription_decoding() {
    let shaping = shaping("gemini");
    let caller: &[(&[u8], &[u8])] = &[(b"content-type", CONTENT_TYPE)];
    for (early, late, expected_type, expected_body) in [
        ("text", "json", "text/plain", "hello"),
        ("json", "text", "application/json", "{\"text\":\"hello\"}"),
    ] {
        let mut body = multipart(early);
        body.truncate(body.len() - b"--fixture--\r\n".len());

        for index in 0..61 {
            body.extend_from_slice(format!("--fixture\r\nContent-Disposition: form-data; name=\"extra{index}\"\r\n\r\nx\r\n").as_bytes());
        }
        body.extend_from_slice(format!("--fixture\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n{late}\r\n--fixture--\r\n").as_bytes());
        let arrived = arrive("POST", "/v1/audio/transcriptions", caller, &body, &shaping).unwrap();
        build(&Host, &arrived, caller, &shaping, "", "model").unwrap();
        let ctx = ReplyCtx {
            arrived: &arrived,
            lane: shaping.lane("model").unwrap(),
            intent: StreamIntent::default(),
            passthrough: false,
            translate_cap: 4096,
        };
        let far = br#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]}}]}"#;
        let mut reply = Reply::new(&Host, &ctx, 200, &[(b"content-type", b"application/json")]);
        let piece = reply.feed(&Host, &ctx, far, true, At::default());
        assert!(piece
            .head
            .unwrap()
            .fields
            .iter()
            .any(|(name, value)| name == "content-type" && value == expected_type.as_bytes()));
        assert_eq!(piece.bytes.as_ref(), expected_body.as_bytes());
    }
}
