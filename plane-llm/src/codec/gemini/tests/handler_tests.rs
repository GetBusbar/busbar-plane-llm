// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/handlers/gemini.rs`.

use super::*;
use crate::codec_diagnostics::{diagnostic_texts, warnings};

#[test]
fn path_model_extracts_the_segment_before_the_last_colon() {
    let h = GeminiRequestHandler;
    assert_eq!(
        h.path_model("/v1beta/models/gemini-2.0-flash:generateContent"),
        Some("gemini-2.0-flash".to_string())
    );
    assert_eq!(
        h.path_model("/v1beta/models/gemini-1.5-pro:streamGenerateContent"),
        Some("gemini-1.5-pro".to_string())
    );

    assert_eq!(h.path_model("/v1beta/foo:bar"), None);

    assert_eq!(h.path_model("/v1beta/models/gemini-2.0-flash"), None);

    assert_eq!(h.path_model("/v1beta/models/:generateContent"), None);
}

#[test]
fn path_base_reshapes_the_gemini_url_for_vertex() {
    let h = GeminiRequestHandler;
    let model = "gemini-2.0-flash";
    let ctx = |path_base| EgressCtx {
        operation: OpVerb::CHAT,
        model,
        stream: false,
        path_base,
    };

    assert_eq!(
        h.upstream_path(&ctx(None)),
        "/v1beta/models/gemini-2.0-flash:generateContent"
    );

    let vbase = "/v1/projects/my-proj/locations/us-central1/publishers/google/models";
    assert_eq!(
        h.upstream_path(&ctx(Some(vbase))),
        "/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash:generateContent"
    );

    assert_eq!(
        h.upstream_path(&EgressCtx {
            operation: OpVerb::EMBEDDINGS,
            model,
            stream: false,
            path_base: Some(vbase),
        }),
        "/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash:embedContent"
    );
}

/// Moderation and rerank (no Gemini cell) answer `:generateContent` whether or not the request
/// streams, as 1.5.5 did (v1.5.5 `crates/busbar/src/handlers/gemini.rs:62-63`); only chat,
/// transcription and speech are stream-aware (`:52-58`).
#[test]
fn moderation_and_rerank_always_use_generate_content() {
    let h = GeminiRequestHandler;
    let model = "gemini-2.0-flash";
    for operation in [OpVerb::MODERATION, OpVerb::RERANK] {
        for stream in [false, true] {
            assert_eq!(
                h.upstream_path(&EgressCtx {
                    operation,
                    model,
                    stream,
                    path_base: None,
                }),
                "/v1beta/models/gemini-2.0-flash:generateContent",
                "{operation:?} stream={stream}"
            );
        }
    }
    for operation in [OpVerb::CHAT, OpVerb::TRANSCRIPTION, OpVerb::SPEECH] {
        assert_eq!(
            h.upstream_path(&EgressCtx {
                operation,
                model,
                stream: true,
                path_base: None,
            }),
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
            "{operation:?}"
        );
    }
}

#[test]
fn transcription_read_request_captures_inline_audio() {
    let audio_b64 = base64_encode(b"pretend-audio-bytes");
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [
            { "text": "please transcribe" },
            { "inline_data": { "mime_type": "audio/wav", "data": audio_b64 } },
        ]}],
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::transcription_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect("valid inline audio body");
    let r = ir;
    let blob = r.audio.expect("audio captured");
    assert_eq!(blob.mime_type, "audio/wav");
    assert_eq!(r.prompt.as_deref(), Some("please transcribe"));
    match blob.payload {
        MediaPayload::B64(s) => assert_eq!(s, base64_encode(b"pretend-audio-bytes")),
        _ => panic!("expected B64 payload"),
    }
}

#[test]
fn transcription_read_request_invalid_base64_is_bad_request() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [
            { "inline_data": { "mime_type": "audio/wav", "data": "!!!not base64!!!" } },
        ]}],
    }))
    .unwrap();
    let err = super::super::super::leaf_codec::transcription_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect_err("invalid base64 must reject");
    assert!(matches!(err, IngressReject::BadRequest(_)));
}

#[test]
fn transcription_read_response_captures_input_and_output_token_counts() {
    let wire = serde_json::to_vec(&json!({
        "candidates": [{ "content": { "parts": [{ "text": "hello" }] } }],
        "usageMetadata": { "promptTokenCount": 11, "candidatesTokenCount": 7 },
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::transcription_read_response("gemini", &wire)
        .expect("valid response");
    let r = ir;
    assert_eq!(r.text, "hello");
    let Some(busbar_contract::billing::Billing::Tokens(usage)) = r.usage else {
        panic!("expected token usage");
    };
    assert_eq!(usage.input, 11);
    assert_eq!(usage.output, 7);
}

#[test]
fn transcription_read_request_without_inline_data_is_bad_request() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [{ "text": "just text" }]}],
    }))
    .unwrap();
    let err = super::super::super::leaf_codec::transcription_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect_err("no audio part must reject");
    match err {
        IngressReject::BadRequest(m) => {
            assert!(m.contains("inline_data audio part"), "message was: {m}");
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[test]
fn resolve_operation_audio_out_is_speech() {
    let h = GeminiRequestHandler;
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [{ "text": "say hi" }]}],
        "generationConfig": { "responseModalities": ["AUDIO"] },
    }))
    .unwrap();
    assert_eq!(
        h.resolve_operation("/v1beta/models/gemini-x:generateContent", &body),
        Some(OpVerb::SPEECH),
    );
}

#[test]
fn resolve_operation_audio_in_is_transcription() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [
            { "inline_data": { "mime_type": "audio/wav", "data": "AAAA" } },
        ]}],
    }))
    .unwrap();
    let h = GeminiRequestHandler;
    assert_eq!(
        h.resolve_operation("/v1beta/models/gemini-x:generateContent", &body),
        Some(OpVerb::TRANSCRIPTION),
    );
}

#[test]
fn resolve_operation_multiple_markers_prefers_speech_over_transcription() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [
            { "inline_data": { "mime_type": "audio/wav", "data": "AAAA" } },
        ]}],
        "generationConfig": { "responseModalities": ["AUDIO"] },
    }))
    .unwrap();
    let h = GeminiRequestHandler;
    assert_eq!(
        h.resolve_operation("/v1beta/models/gemini-x:generateContent", &body),
        Some(OpVerb::SPEECH),
    );
}

#[test]
fn resolve_operation_no_markers_at_all_is_chat() {
    let h = GeminiRequestHandler;
    assert_eq!(
        h.resolve_operation(
            "/v1beta/models/gemini-x:generateContent",
            b"not json at all"
        ),
        Some(OpVerb::CHAT),
    );
}

#[test]
fn resolve_operation_plain_text_is_chat() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "role": "user", "parts": [{ "text": "hello" }]}],
    }))
    .unwrap();
    let h = GeminiRequestHandler;
    assert_eq!(
        h.resolve_operation("/v1beta/models/gemini-x:generateContent", &body),
        Some(OpVerb::CHAT),
    );
}

#[test]
fn embeddings_read_request_captures_content_text() {
    let body = serde_json::to_vec(&json!({
        "content": { "parts": [{ "text": "embed me" }] },
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::embeddings_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect("valid embedContent body");
    let r = ir;
    assert_eq!(r.input, EmbInput::Text(vec!["embed me".to_string()]));
}

#[test]
fn embeddings_read_request_without_text_is_bad_request() {
    let body = serde_json::to_vec(&json!({ "content": { "parts": [] } })).unwrap();
    let err = super::super::super::leaf_codec::embeddings_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect_err("empty content must reject");
    assert!(matches!(err, IngressReject::BadRequest(_)));
}

#[test]
fn image_read_request_captures_prompt_and_count() {
    let body = serde_json::to_vec(&json!({
        "instances": [{ "prompt": "a fox" }],
        "parameters": { "sampleCount": 3 },
    }))
    .unwrap();
    let ir =
        super::super::super::leaf_codec::image_read_request("gemini", &body, "application/json")
            .expect("valid predict body");
    let r = ir;
    assert_eq!(r.prompt.as_deref(), Some("a fox"));
    assert_eq!(r.n, Some(3));
}

#[test]
fn image_read_request_captures_aspect_ratio() {
    let body = serde_json::to_vec(&json!({
        "instances": [{ "prompt": "a fox" }],
        "parameters": { "aspectRatio": "16:9" },
    }))
    .unwrap();
    let ir =
        super::super::super::leaf_codec::image_read_request("gemini", &body, "application/json")
            .expect("valid predict body");
    let r = ir;
    assert_eq!(r.aspect_ratio.as_deref(), Some("16:9"));
}

#[test]
fn image_read_response_captures_base64_image_bytes() {
    let wire = serde_json::to_vec(&json!({
        "predictions": [{ "bytesBase64Encoded": "cHJldGVuZC1pbWFnZQ==", "mimeType": "image/png" }],
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::image_read_response("gemini", &wire)
        .expect("valid predictions body");
    let r = ir;
    assert_eq!(r.images.len(), 1);
    assert_eq!(r.images[0].b64.as_deref(), Some("cHJldGVuZC1pbWFnZQ=="));
    assert_eq!(r.images[0].mime_type.as_deref(), Some("image/png"));
}

#[test]
fn image_response_usage_metadata_is_recorded_never_billed() {
    let wire = serde_json::to_vec(&json!({
        "predictions": [{ "bytesBase64Encoded": "AAAA", "mimeType": "image/png" }],
        "usageMetadata": { "promptTokenCount": 20, "candidatesTokenCount": 10 },
    }))
    .unwrap();
    let resp = super::read_image_response(&wire).unwrap();
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Tokens(t)) => {
            assert_eq!((t.input, t.output), (0, 0), "no image token bills: {t:?}");
            assert_eq!(
                t.residual_units,
                std::collections::BTreeMap::from([
                    ("image.input_tokens".to_string(), 20),
                    ("image.output_tokens".to_string(), 10),
                ])
            );
        }
        other => {
            panic!("a token-reporting Gemini image answer is a zero token line, got {other:?}")
        }
    }
}

#[test]
fn image_response_without_usage_bills_per_image() {
    let wire = serde_json::to_vec(&json!({
        "predictions": [
            { "bytesBase64Encoded": "AAAA", "mimeType": "image/png" },
            { "bytesBase64Encoded": "BBBB", "mimeType": "image/png" },
        ],
    }))
    .unwrap();
    let resp = super::read_image_response(&wire).unwrap();
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Images { count, .. }) => {
            assert_eq!(count, 2)
        }
        other => panic!("per-image Imagen response must bill Images, got {other:?}"),
    }
}

#[test]
fn image_write_read_roundtrip_preserves_prompt() {
    let req = crate::codec::ir::image::ImageReq {
        prompt: Some("roundtrip fox".to_string()),
        n: Some(2),
        ..Default::default()
    };
    let wire = super::super::super::leaf_codec::image_write_request("gemini", &req);
    let back =
        super::super::super::leaf_codec::image_read_request("gemini", &wire, "application/json")
            .expect("emitted body reparses");
    let r = back;
    assert_eq!(r.prompt.as_deref(), Some("roundtrip fox"));
    assert_eq!(r.n, Some(2));
}

#[test]
fn embeddings_write_request_carries_dimensions_task_type_and_title() {
    let ir = crate::codec::ir::embeddings::EmbeddingsReq {
        input: EmbInput::Text(vec!["hi".into()]),
        dimensions: Some(256),
        task_type: Some("RETRIEVAL_DOCUMENT".into()),
        title: Some("doc".into()),
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::embeddings_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["outputDimensionality"], 256);
    assert_eq!(v["taskType"], "RETRIEVAL_DOCUMENT");
    assert_eq!(v["title"], "doc");
}

#[test]
fn embeddings_taps_usage_is_true() {
    assert!(EMB.taps_usage());
}

#[test]
fn embeddings_read_request_captures_task_type() {
    let body = serde_json::to_vec(&json!({
        "content": { "parts": [{ "text": "hi" }] },
        "taskType": "RETRIEVAL_QUERY",
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::embeddings_read_request(
        "gemini",
        &body,
        "application/json",
    )
    .expect("valid embedContent body");
    let r = ir;
    assert_eq!(r.task_type.as_deref(), Some("RETRIEVAL_QUERY"));
}

#[test]
fn embeddings_read_response_captures_vector_and_usage() {
    let wire = serde_json::to_vec(&json!({
        "embedding": { "values": [0.1, 0.2, 0.3] },
        "usageMetadata": { "promptTokenCount": 5 },
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::embeddings_read_response("gemini", &wire)
        .expect("valid embedContent response");
    let r = ir;
    assert_eq!(r.embeddings.len(), 1);
    match r.embeddings[0].vectors.get(&EncFmt::Float) {
        Some(VectorData::Float(v)) => assert_eq!(v, &[0.1_f32, 0.2, 0.3]),
        other => panic!("expected a Float vector, got {other:?}"),
    }
    assert_eq!(r.usage.expect("usage present").input, 5);
}

#[test]
fn embeddings_write_response_emits_the_float_vector() {
    let mut item = EmbeddingItem::default();
    item.vectors
        .insert(EncFmt::Float, VectorData::Float(vec![1.0, 2.0, 3.0]));
    let ir = EmbeddingsResp {
        embeddings: vec![item],
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::embeddings_write_response("gemini", &ir);
    let v: Value = serde_json::from_slice(&out.bytes).unwrap();
    assert_eq!(v["embedding"]["values"], json!([1.0, 2.0, 3.0]));
}

#[test]
fn embeddings_write_request_warns_on_dropped_non_text_input() {
    let ir = crate::codec::ir::embeddings::EmbeddingsReq {
        input: EmbInput::Images(vec!["data:image/png;base64,AA==".into()]),
        ..Default::default()
    };
    let (cap, cap_subscriber) = warnings();
    let subscriber = cap_subscriber.clone();
    let out = tracing::dispatcher::with_default(&subscriber, || {
        super::super::super::leaf_codec::embeddings_write_request("gemini", &ir)
    });

    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["content"]["parts"][0]["text"], json!(""));

    assert!(
        diagnostic_texts(&cap)
            .iter()
            .any(|event| event.contains("dropping a non-text embeddings input")),
        "a dropped non-text embeddings input must warn: {:?}",
        diagnostic_texts(&cap)
    );
}

#[test]
fn image_write_request_carries_aspect_ratio_and_person_generation() {
    let ir = crate::codec::ir::image::ImageReq {
        prompt: Some("a fox".into()),
        aspect_ratio: Some("16:9".into()),
        person_generation: Some("allow_adult".into()),
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::image_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["parameters"]["aspectRatio"], "16:9");
    assert_eq!(v["parameters"]["personGeneration"], "allow_adult");
}

#[test]
fn speech_read_response_invalid_base64_is_malformed() {
    let body = br#"{"candidates":[{"content":{"parts":[{"inlineData":{"data":"!!!not-base64!!!","mimeType":"audio/L16;codec=pcm;rate=24000"}}]}}]}"#;
    let res = super::super::super::leaf_codec::speech_read_response("gemini", body);
    assert!(matches!(res, Err(CodecError::Malformed(_))));
}

#[test]
fn speech_read_response_valid_base64_returns_audio_blob() {
    let data = base64_encode(b"pretend-pcm-audio");
    let body = serde_json::to_vec(&json!({
        "candidates": [{ "content": { "parts": [{ "inlineData": {
            "data": data, "mimeType": "audio/L16;codec=pcm;rate=24000",
        } }] } }],
    }))
    .unwrap();
    let ir = super::super::super::leaf_codec::speech_read_response("gemini", &body)
        .expect("valid base64 must decode");
    let r = ir;
    let blob = r.audio.expect("audio blob present");
    match blob.payload {
        MediaPayload::B64(s) => assert_eq!(s, base64_encode(b"pretend-pcm-audio")),
        _ => panic!("expected B64 payload"),
    }
}

#[test]
fn speech_read_request_captures_prompt_text() {
    let body = serde_json::to_vec(&json!({
        "contents": [{ "parts": [{ "text": "hello" }] }],
        "generationConfig": { "responseModalities": ["AUDIO"] },
    }))
    .unwrap();
    let ir =
        super::super::super::leaf_codec::speech_read_request("gemini", &body, "application/json")
            .expect("valid speech body");
    let r = ir;
    assert_eq!(r.input, "hello");
}

#[test]
fn speech_write_request_prefixes_instructions_to_prompt_not_language_code() {
    let ir = crate::codec::ir::audio::SpeechReq {
        input: "hello".into(),
        timbre: "Kore".into(),
        instructions: Some("speak cheerfully".into()),
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::speech_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let text = v
        .pointer("/contents/0/parts/0/text")
        .and_then(Value::as_str)
        .expect("prompt text present");
    assert_eq!(text, "speak cheerfully: hello");
    assert!(
        !serde_json::to_string(&v).unwrap().contains("languageCode"),
        "instructions must not corrupt speechConfig.languageCode: {v}"
    );
}

#[test]
fn gemini_speech_response_is_billed() {
    let resp = super::read_speech_response(
        br#"{"candidates":[{"content":{"parts":[{"inlineData":{"data":"AAE=","mimeType":"audio/L16;codec=pcm;rate=24000"}}]}}]}"#,
    )
    .unwrap();
    let audio = resp.audio.as_ref().expect("inline audio");
    assert_eq!(audio.mime_type, "audio/L16;codec=pcm;rate=24000");
    assert!(matches!(&audio.payload, MediaPayload::B64(data) if data == "AAE="));
    assert!(
        resp.billing().is_some(),
        "gemini TTS synthesis must be billed (non-None), got None"
    );
}

#[test]
fn a_json_error_shaped_speech_body_is_not_served_as_audio() {
    for body in [
        br#"{"error":{"code":400,"message":"invalid voice","status":"INVALID_ARGUMENT"}}"#.to_vec(),
        br#"{"candidates":[{"finishReason":"SAFETY","content":{"parts":[]}}]}"#.to_vec(),
        br#"{"candidates":[{"content":{"parts":[{"text":"I cannot do that."}]}}]}"#.to_vec(),
    ] {
        let got = super::read_speech_response(&body);
        assert!(
            matches!(got, Err(CodecError::Malformed(_))),
            "a JSON body with no inlineData is not audio: {}",
            String::from_utf8_lossy(&body)
        );
    }

    for raw in [
        b"\xff\xfbraw-mp3".as_slice(),
        b"\x00\x01raw-audio".as_slice(),
    ] {
        assert!(
            matches!(
                super::read_speech_response(raw),
                Err(CodecError::Malformed(_))
            ),
            "Gemini requires a JSON inlineData envelope, even for recognizable audio bytes"
        );
    }
}

#[test]
fn a_duration_billed_transcription_writes_no_usage_metadata() {
    let ir = TranscriptionResp {
        text: "hi".into(),
        usage: Some(busbar_contract::billing::Billing::Duration {
            seconds: busbar_contract::Count::parse("12.5").expect("12.5 is an exact decimal"),
        }),
        ..Default::default()
    };
    let out = super::write_transcription_response(&ir).bytes;
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(v.get("usageMetadata").is_none(), "{v}");
}

#[test]
fn gemini_transcription_forwards_caller_prompt_and_temperature() {
    let ir = crate::codec::ir::audio::TranscriptionReq {
        model: "gemini-2.0-flash".into(),
        prompt: Some("Spell Busbar correctly.".into()),
        temperature: Some(0.5),
        audio: Some(crate::codec::framework::MediaBlob {
            payload: crate::codec::framework::MediaPayload::Bytes(
                busbar_contract::SlabBytes::from(&b"x"[..]),
            ),
            mime_type: "audio/mpeg".into(),
            pcm: None,
        }),
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::transcription_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let parts = v
        .pointer("/contents/0/parts")
        .and_then(Value::as_array)
        .expect("parts array");
    assert!(
        parts
            .iter()
            .any(|p| p.get("text").and_then(Value::as_str) == Some("Spell Busbar correctly.")),
        "the caller's prompt must be forwarded as a text part, not replaced by the directive: {v}"
    );
    assert_eq!(
        v.pointer("/generationConfig/temperature")
            .and_then(Value::as_f64),
        Some(0.5),
        "temperature must be forwarded via generationConfig: {v}"
    );

    let back = super::super::super::leaf_codec::transcription_read_request(
        "gemini",
        &out,
        "application/json",
    )
    .expect("re-read");
    assert_eq!(back.prompt.as_deref(), Some("Spell Busbar correctly."));
    assert_eq!(back.temperature, Some(0.5));
}

#[test]
fn gemini_speech_two_speaker_request_round_trips() {
    let ir = crate::codec::ir::audio::SpeechReq {
        model: "gemini-2.5-flash-preview-tts".into(),
        input: "Joe: Hi. Jane: Hello.".into(),
        speakers: vec![
            ("Joe".into(), "Kore".into()),
            ("Jane".into(), "Puck".into()),
        ],
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::speech_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let configs = v
        .pointer("/generationConfig/speechConfig/multiSpeakerVoiceConfig/speakerVoiceConfigs")
        .and_then(Value::as_array)
        .expect("speakerVoiceConfigs must be emitted");
    assert_eq!(configs.len(), 2, "both speakers must be emitted: {v}");
    assert_eq!(configs[0]["speaker"], "Joe");
    assert_eq!(
        configs[1].pointer("/voiceConfig/prebuiltVoiceConfig/voiceName"),
        Some(&json!("Puck"))
    );
    let back =
        super::super::super::leaf_codec::speech_read_request("gemini", &out, "application/json")
            .expect("re-read");
    assert_eq!(
        back.speakers,
        vec![
            ("Joe".to_string(), "Kore".to_string()),
            ("Jane".to_string(), "Puck".to_string())
        ]
    );
}

#[test]
fn gemini_imagen_image_carries_negative_prompt_seed_guidance_and_size_tier() {
    let ir = crate::codec::ir::image::ImageReq {
        model: "imagen-3.0-generate-002".into(),
        prompt: Some("a serene lake".into()),
        negative_prompt: Some("no boats".into()),
        seed: Some(42),
        guidance_scale: Some(7.5),
        image_size_tier: Some("2K".into()),
        ..Default::default()
    };
    let out = super::super::super::leaf_codec::image_write_request("gemini", &ir);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v.pointer("/parameters/negativePrompt"),
        Some(&json!("no boats")),
        "{v}"
    );
    assert_eq!(v.pointer("/parameters/seed"), Some(&json!(42)));
    assert_eq!(v.pointer("/parameters/sampleImageSize"), Some(&json!("2K")));
    let back =
        super::super::super::leaf_codec::image_read_request("gemini", &out, "application/json")
            .expect("re-read");
    assert_eq!(back.negative_prompt.as_deref(), Some("no boats"));
    assert_eq!(back.seed, Some(42));
    assert_eq!(back.guidance_scale, Some(7.5));
    assert_eq!(back.image_size_tier.as_deref(), Some("2K"));
}

#[test]
fn transcription_translate_round_trips_as_translate() {
    let req = crate::codec::ir::audio::TranscriptionReq {
        audio: Some(MediaBlob {
            payload: MediaPayload::B64(base64_encode(b"audio-bytes")),
            mime_type: "audio/mp3".into(),
            pcm: None,
        }),
        target_language: Some("en".into()),
        ..Default::default()
    };

    let wire = write_transcription_request(&req);
    let v: Value = serde_json::from_slice(&wire).unwrap();
    let first_text = v
        .pointer("/contents/0/parts/0/text")
        .and_then(Value::as_str);
    assert_eq!(
        first_text,
        Some(TRANSLATE_INSTRUCTION),
        "translate request must emit the translate directive: {v}"
    );

    let back = read_transcription_request(&wire, "application/json").expect("re-read");
    assert!(
        back.target_language.is_some(),
        "TRANSLATE_INSTRUCTION must reconstruct target_language (translate mode), got None"
    );

    let wire2 = write_transcription_request(&back);
    let v2: Value = serde_json::from_slice(&wire2).unwrap();
    assert_eq!(
        v2.pointer("/contents/0/parts/0/text")
            .and_then(Value::as_str),
        Some(TRANSLATE_INSTRUCTION),
        "re-emit must stay translate, not downgrade to transcribe: {v2}"
    );
}

#[test]
fn transcription_transcribe_round_trips_as_transcribe() {
    let req = crate::codec::ir::audio::TranscriptionReq {
        audio: Some(MediaBlob {
            payload: MediaPayload::B64(base64_encode(b"audio-bytes")),
            mime_type: "audio/mp3".into(),
            pcm: None,
        }),
        ..Default::default()
    };
    let wire = write_transcription_request(&req);
    let back = read_transcription_request(&wire, "application/json").expect("re-read");
    assert!(
        back.target_language.is_none(),
        "a transcribe request must not gain a target_language"
    );
}

#[test]
fn transcription_response_reads_no_audio_duration_member() {
    let wire = json!({
        "candidates": [{"content": {"parts": [{"text": "hello"}], "role": "model"}}],
        "usageMetadata": {"audioDurationSeconds": 12.5},
    });
    let back = read_transcription_response(&serde_json::to_vec(&wire).unwrap()).expect("read");
    assert_eq!(
        back.usage,
        Some(busbar_contract::billing::Billing::Tokens(
            busbar_contract::billing::TokenUsage {
                exact_counts: Some(Default::default()),
                ..Default::default()
            }
        ))
    );
}

#[test]
fn transcription_response_reads_token_usage() {
    let wire = json!({
        "candidates": [{"content": {"parts": [{"text": "hi"}], "role": "model"}}],
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3},
    });
    let back = read_transcription_response(&serde_json::to_vec(&wire).unwrap()).expect("read");
    assert_eq!(
        back.usage,
        Some(busbar_contract::billing::Billing::Tokens(
            busbar_contract::billing::TokenUsage {
                exact_counts: Some(
                    [
                        (
                            "input".to_owned(),
                            busbar_contract::Count::parse("7").unwrap()
                        ),
                        (
                            "output".to_owned(),
                            busbar_contract::Count::parse("3").unwrap()
                        ),
                    ]
                    .into()
                ),
                input: 7,
                output: 3,
                ..Default::default()
            }
        )),
        "token usageMetadata must read back as Tokens"
    );
}

#[test]
fn image_response_writer_writes_no_usage_metadata() {
    let resp = crate::codec::ir::image::ImageResp {
        images: vec![crate::codec::framework::ImageOutput {
            b64: Some("aGVsbG8=".into()),
            ..Default::default()
        }],
        usage: Some(crate::codec::ir::image::ImageTokens {
            input: Some(10),
            output: Some(5),
        }),
        ..Default::default()
    };
    let wb = write_image_response(&resp);
    let v: Value = serde_json::from_slice(&wb.bytes).unwrap();
    assert!(v.get("usageMetadata").is_none(), "{v}");
    assert_eq!(
        v.pointer("/predictions/0/bytesBase64Encoded"),
        Some(&json!("aGVsbG8="))
    );
}

#[test]
fn image_response_writer_omits_usage_when_absent() {
    let resp = crate::codec::ir::image::ImageResp {
        images: vec![crate::codec::framework::ImageOutput {
            b64: Some("aGVsbG8=".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let wb = write_image_response(&resp);
    let v: Value = serde_json::from_slice(&wb.bytes).unwrap();
    assert!(
        v.get("usageMetadata").is_none(),
        "a usage-less image response must not fabricate usageMetadata: {v}"
    );
}

#[test]
fn embeddings_refuse_every_malformed_coordinate_and_recover_usage_independently() {
    for embedding in [
        json!(null),
        json!({}),
        json!({"values":{}}),
        json!({"values":[0.1,null,0.3]}),
        json!({"values":[0.1,"0.2",0.3]}),
        json!({"values":[0.1,1e100,0.3]}),
    ] {
        let body = serde_json::to_vec(
            &json!({"embedding":embedding,"usageMetadata":{"promptTokenCount":5}}),
        )
        .unwrap();
        assert!(EMB
            .read_response(crate::codec::test_host::HOST, &body)
            .is_err());
        let usage = EMB
            .extract_usage(crate::codec::test_host::HOST, "gemini", &body)
            .unwrap()
            .unwrap();
        assert_eq!(usage.input, 5);
        assert_eq!(
            EMB.recover_response_billing(crate::codec::test_host::HOST, &body),
            Some(busbar_contract::billing::Billing::Tokens(usage))
        );
    }

    let response = read_embeddings_response(br#"{"embedding":{"values":[]}}"#).unwrap();
    assert_eq!(
        response.embeddings[0].vectors.get(&EncFmt::Float),
        Some(&VectorData::Float(vec![]))
    );
}
