use super::*;

#[test]
fn transcription_keeps_audio_when_an_image_follows() {
    let body = br#"{"contents":[{"parts":[{"inlineData":{"mimeType":"audio/wav","data":"YQ=="}},{"inlineData":{"mimeType":"image/png","data":"Yg=="}}]}]}"#;
    let request = handler::read_transcription_request(body, "application/json").unwrap();
    let audio = request.audio.unwrap();
    assert_eq!(audio.mime_type, "audio/wav");
    assert!(
        matches!(audio.payload, crate::codec::framework::MediaPayload::B64(data) if data == "YQ==")
    );
}

#[test]
fn speech_refuses_truncated_json() {
    assert!(handler::read_speech_response(br#"{"candidates":"#).is_err());
}

#[test]
fn array_stream_reports_truncated_tail() {
    for translated in [false, true] {
        let mut framer = GeminiJsonArrayFramer::new();
        let mut bytes = framer.feed(b"data: {\"text\":\"first\"}\n\ndata: {\"text\":");
        bytes.extend(if translated {
            framer.finish_for_translate(false)
        } else {
            framer.finish()
        });
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["text"], "first");
        assert_eq!(value[1]["error"]["status"], "INTERNAL");
        assert!(framer.finish().is_empty());
    }
}

#[test]
fn image_guidance_scale_refuses_float_overflow() {
    assert!(handler::read_image_request(
        br#"{"parameters":{"guidanceScale":1e100}}"#,
        "application/json"
    )
    .is_err());
    assert_eq!(
        handler::read_image_request(
            br#"{"parameters":{"guidanceScale":7.5}}"#,
            "application/json"
        )
        .unwrap()
        .guidance_scale,
        Some(7.5)
    );
}

#[test]
fn transcription_preserves_all_prompt_parts() {
    let body = br#"{"contents":[{"parts":[{"text":"Translate into French."},{"text":"Keep speaker labels."},{"inlineData":{"mimeType":"audio/wav","data":"YQ=="}}]}]}"#;
    let request = handler::read_transcription_request(body, "application/json").unwrap();
    assert_eq!(
        request.prompt.as_deref(),
        Some("Translate into French.\nKeep speaker labels.")
    );
}
