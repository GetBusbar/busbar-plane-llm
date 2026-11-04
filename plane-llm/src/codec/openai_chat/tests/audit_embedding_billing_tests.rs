use super::*;
#[test]
fn refused_embedding_retains_provider_billing() {
    let body = br#"{"data":[{"embedding":[0.1,"bad",0.3]}],"usage":{"prompt_tokens":5}}"#;
    assert!(EMBEDDINGS
        .read_response(crate::codec::test_host::HOST, body)
        .is_err());
    let usage = EMBEDDINGS
        .extract_usage(crate::codec::test_host::HOST, "openai_chat", body)
        .unwrap()
        .unwrap();
    assert_eq!(usage.input, 5);
    assert_eq!(
        EMBEDDINGS.recover_response_billing(crate::codec::test_host::HOST, body),
        Some(busbar_contract::billing::Billing::Tokens(usage))
    );
}

#[test]
fn embeddings_preserve_finite_components_and_refuse_overflow() {
    assert!(read_embeddings_response(br#"{"data":[{"embedding":[1e100]}]}"#).is_err());
    let response = read_embeddings_response(br#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#).unwrap();
    assert_eq!(
        response.embeddings[0].vectors.get(&EncFmt::Float),
        Some(&VectorData::Float(vec![0.1, 0.2, 0.3]))
    );
}

#[test]
fn plain_transcription_that_looks_like_json_retains_wire_text() {
    for text in [
        "123",
        "true",
        "\"hello\"",
        "[1,2]",
        "{\"word\":1}",
        "{\"text\":42}",
        "{\"text\":null}",
        "{\"text\":{}}",
    ] {
        let response = read_transcription_response(text.as_bytes()).unwrap();
        assert_eq!(response.text, text);
        assert_eq!(response.response_format.as_deref(), Some("text"));
    }
}

#[test]
fn transcription_prompt_preserves_lines_and_refuses_delimiters() {
    let mut request = TranscriptionReq {
        model: "whisper-1".into(),
        prompt: Some("Alice:\r\nBob:\nCarol:".into()),
        ..Default::default()
    };
    let body = write_transcription_request(&request);
    let fields = parse_multipart(&body, TRANSCRIPTION_CONTENT_TYPE);
    let prompt = fields.iter().find(|field| field.name == "prompt").unwrap();
    assert_eq!(prompt.value, b"Alice:\r\nBob:\nCarol:");
    for prefix in ["", "\r\n", "\n"] {
        request.prompt = Some(format!("{prefix}--{TRANSCRIPTION_BOUNDARY}\r\ninjected"));
        assert!(write_transcription_request(&request).is_empty());
    }
}

#[test]
fn moderation_requires_explicit_boolean_verdicts() {
    for body in [
        br#"{"results":[null]}"#.as_slice(),
        br#"{"results":[{"flagged":"true"}]}"#.as_slice(),
        br#"{"results":[{}]}"#.as_slice(),
        br#"{}"#.as_slice(),
    ] {
        assert!(read_moderation_response(body).is_err());
    }
    for flagged in [false, true] {
        let body =
            serde_json::to_vec(&serde_json::json!({"results":[{"flagged":flagged}]})).unwrap();
        assert_eq!(
            read_moderation_response(&body).unwrap().results[0].flagged,
            flagged
        );
    }
}

#[test]
fn malformed_embedding_shapes_fail_but_preserve_reported_usage() {
    for mut body in [
        serde_json::json!({}),
        serde_json::json!({"data":null}),
        serde_json::json!({"data":[null]}),
        serde_json::json!({"data":[{"embedding":null}]}),
    ] {
        body["usage"] = serde_json::json!({"prompt_tokens":9});
        let wire = serde_json::to_vec(&body).unwrap();
        assert!(read_embeddings_response(&wire).is_err());
        let billing = EMBEDDINGS
            .extract_usage(crate::codec::test_host::HOST, "openai_chat", &wire)
            .unwrap()
            .unwrap();
        assert_eq!(billing.input, 9);
    }
    assert!(read_embeddings_response(br#"{"data":[]}"#)
        .unwrap()
        .embeddings
        .is_empty());
    assert_eq!(
        read_embeddings_response(br#"{"data":[{"embedding":[]},{"embedding":"AAAAAA=="}]}"#)
            .unwrap()
            .embeddings
            .len(),
        2
    );
}
