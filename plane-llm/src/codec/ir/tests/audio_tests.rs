// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `crates/busbar/src/ir/audio.rs`.

use super::*;
use crate::codec::framework::MediaBlob;
use crate::codec::framework::MediaPayload;

#[test]
fn transcription_translation_folds_via_target_language() {
    let transcribe = TranscriptionReq {
        model: "whisper-1".into(),
        ..Default::default()
    };
    assert!(
        transcribe.target_language.is_none(),
        "no target = transcribe"
    );
    let translate = TranscriptionReq {
        model: "whisper-1".into(),
        target_language: Some("en".into()),
        ..Default::default()
    };
    assert_eq!(
        translate.target_language.as_deref(),
        Some("en"),
        "target set = translate"
    );
}

#[test]
fn transcription_billing_is_model_dependent() {
    let whisper = TranscriptionResp {
        text: "hi".into(),
        usage: Some(Billing::Duration {
            seconds: busbar_contract::Count::parse("3.2").expect("3.2 is an exact decimal"),
        }),
        ..Default::default()
    };
    assert!(matches!(whisper.billing(), Some(Billing::Duration { .. })));
}

#[test]
fn speech_carries_binary_out_and_char_or_token_billing() {
    let resp = SpeechResp {
        audio: Some(MediaBlob {
            payload: MediaPayload::Bytes(busbar_contract::SlabBytes::from(&b"\xff\xfb"[..])),
            mime_type: "audio/mpeg".into(),
            pcm: None,
        }),
        usage: Some(Billing::Characters { count: 11 }),
        ..Default::default()
    };
    assert!(resp.audio.is_some());
    assert!(matches!(
        resp.billing(),
        Some(Billing::Characters { count: 11 })
    ));
}

use busbar_contract::ir::facts::{ContentItem, IrFacts, OPAQUE_CONTENT_MARKER};
use busbar_contract::operation::OpVerb;

use crate::codec::test_host::screened;

#[test]
fn transcription_projects_prompt_as_text_and_audio_as_opaque() {
    let req = TranscriptionReq {
        audio: Some(crate::codec::framework::MediaBlob {
            payload: crate::codec::framework::MediaPayload::B64("AAAA".into()),
            mime_type: "audio/mp3".into(),
            pcm: None,
        }),
        model: "whisper-1".into(),
        prompt: Some("proper nouns to expect".into()),
        stream: true,
        ..Default::default()
    };
    assert_eq!(IrFacts::verb(&req), OpVerb::TRANSCRIPTION);
    assert!(IrFacts::wants_stream(&req));
    let items = req.content();

    assert!(matches!(items[0], ContentItem::Opaque { .. }));
    assert_eq!(items[0].screenable_text(), OPAQUE_CONTENT_MARKER);
    assert!(screened(&items)
        .iter()
        .any(|t| t == "proper nouns to expect"));
}

#[test]
fn speech_projects_input_instructions_and_speaker_names() {
    let req = SpeechReq {
        input: "hello world".into(),
        model: "gpt-4o-mini-tts".into(),
        timbre: "alloy".into(),
        instructions: Some("speak cheerfully".into()),
        speakers: vec![("Dr. Smith".into(), "verse".into())],
        stream: false,
        ..Default::default()
    };
    assert_eq!(IrFacts::verb(&req), OpVerb::SPEECH);
    let screened = screened(&req.content());

    assert!(screened.iter().any(|t| t == "hello world"));
    assert!(screened.iter().any(|t| t == "speak cheerfully"));
    assert!(screened.iter().any(|t| t == "Dr. Smith"));

    assert!(!screened.iter().any(|t| t == "verse"));
}

#[test]
fn speech_instructions_alone_are_screened() {
    let req = SpeechReq {
        input: "x".into(),
        instructions: Some("SECRET-INSTRUCTIONS".into()),
        ..Default::default()
    };
    assert!(screened(&req.content())
        .iter()
        .any(|t| t == "SECRET-INSTRUCTIONS"));
}

#[test]
fn speech_request_meters_by_character_count() {
    let req = SpeechReq {
        input: "hello world".into(),
        model: "tts-1".into(),
        timbre: "alloy".into(),
        ..Default::default()
    };
    assert_eq!(
        req.billing(),
        Some(Billing::Characters { count: 11 }),
        "TTS must meter by the exact input character count, not Flat"
    );
}

#[test]
fn speech_request_character_count_is_unicode_scalar_not_bytes() {
    let req = SpeechReq {
        input: "café €".into(),
        ..Default::default()
    };
    assert_eq!(
        req.billing(),
        Some(Billing::Characters { count: 6 }),
        "character count must be Unicode scalars, not bytes"
    );
}

#[test]
fn speech_empty_input_meters_zero_characters() {
    let req = SpeechReq::default();
    assert_eq!(req.billing(), Some(Billing::Characters { count: 0 }));
}
