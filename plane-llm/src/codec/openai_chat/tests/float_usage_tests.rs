// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! FLOAT-ENCODED USAGE COUNTS, PROVEN THROUGH `read_count_u64`.

use super::*;
use crate::codec::ir::StreamDecodeState;

/// THE TRUNCATED-STREAM RECOVERY PATH. `recover_truncated_usage` parses the trailing `usage` object

#[test]
fn recover_truncated_usage_reads_float_encoded_counts() {
    let reader = OpenAiReader;
    let tail = br#"{"usage":{"prompt_tokens":1000.0,"completion_tokens":50.0,"prompt_tokens_details":{"cached_tokens":200.0}}}"#;
    let usage = reader
        .recover_truncated_usage(tail)
        .expect("usage tail must be recoverable");
    assert_eq!(
        usage.input, 800,
        "prompt_tokens(1000.0) - cached_tokens(200.0), both float-spelled"
    );
    assert_eq!(usage.output, 50, "completion_tokens(50.0), float-spelled");
    assert_eq!(usage.cache_read, Some(200));
}

/// THE STREAMING PATH: a `stream_options: {include_usage: true}` trailing usage-only chunk

#[test]
fn streaming_trailing_usage_chunk_reads_float_encoded_counts() {
    let reader = OpenAiReader;
    let mut state = StreamDecodeState::default();
    let chunk = serde_json::json!({
        "id": "chatcmpl-float9",
        "object": OBJ_CHUNK,
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [],
        "usage": {
            "prompt_tokens": 1000.0,
            "completion_tokens": 50.0,
            "prompt_tokens_details": {"cached_tokens": 200.0, "audio_tokens": 3.0},
            "completion_tokens_details": {
                "reasoning_tokens": 400.0,
                "audio_tokens": 7.0,
                "accepted_prediction_tokens": 11.0,
                "rejected_prediction_tokens": 2.0
            }
        }
    });
    let events = reader.read_response_events(crate::codec::test_host::HOST, "", &chunk, &mut state);
    let usage = events
        .into_iter()
        .find_map(|e| match e {
            crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("the trailing usage-only chunk must emit a MessageDelta carrying usage");
    assert_eq!(
        usage.input_tokens, 800,
        "prompt_tokens(1000.0) - cached_tokens(200.0), streamed trailer, both float-spelled"
    );
    assert_eq!(
        usage.output_tokens, 50,
        "completion_tokens(50.0), streamed trailer"
    );
    assert_eq!(usage.cache_read_input_tokens, Some(200));
    assert_eq!(usage.detail.reasoning_tokens, Some(400));
    assert_eq!(usage.detail.input_audio_tokens, Some(3));
    assert_eq!(usage.detail.output_audio_tokens, Some(7));
    assert_eq!(usage.detail.accepted_prediction_tokens, Some(11));
    assert_eq!(usage.detail.rejected_prediction_tokens, Some(2));
}

/// THE BUFFERED PATH. The identical float-spelled usage object on a non-streamed `chat.completion`

#[test]
fn buffered_read_response_reads_float_encoded_counts() {
    let body = serde_json::json!({
        "id": "chatcmpl-float10",
        "object": "chat.completion",
        "created": 1u64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": FINISH_STOP
        }],
        "usage": {
            "prompt_tokens": 1000.0,
            "completion_tokens": 50.0,
            "prompt_tokens_details": {"cached_tokens": 200.0, "audio_tokens": 3.0},
            "completion_tokens_details": {
                "reasoning_tokens": 400.0,
                "audio_tokens": 7.0,
                "accepted_prediction_tokens": 11.0,
                "rejected_prediction_tokens": 2.0
            }
        }
    });
    let ir = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .expect("read_response");
    assert_eq!(
        ir.usage.input_tokens, 800,
        "prompt_tokens(1000.0) - cached_tokens(200.0), buffered, both float-spelled"
    );
    assert_eq!(
        ir.usage.output_tokens, 50,
        "completion_tokens(50.0), buffered"
    );
    assert_eq!(ir.usage.cache_read_input_tokens, Some(200));
    assert_eq!(ir.usage.detail.reasoning_tokens, Some(400));
    assert_eq!(ir.usage.detail.input_audio_tokens, Some(3));
    assert_eq!(ir.usage.detail.output_audio_tokens, Some(7));
    assert_eq!(ir.usage.detail.accepted_prediction_tokens, Some(11));
    assert_eq!(ir.usage.detail.rejected_prediction_tokens, Some(2));
}

/// AN UNREADABLE IMAGE COUNT IS RECORDED AS UNKNOWN: THE ANSWER IS SERVED, THE COUNT NEVER 0.

#[test]
fn an_unreadable_image_count_is_recorded_as_unknown_and_the_answer_served() {
    let wire = br#"{"data":[{"b64_json":"aGk="}],"usage":{"input_tokens":"27","output_tokens":5}}"#;
    let resp = crate::codec::openai_chat::handler::read_image_response(wire)
        .expect("the answer is served whatever its usage said, as in 1.5.5");
    assert_eq!(resp.images.len(), 1, "the image is served");
    assert_eq!(
        resp.usage,
        Some(crate::codec::ir::image::ImageTokens {
            input: None,
            output: Some(5),
        }),
        "the unreadable count is unknown, not 0; the readable one is read"
    );
    assert!(
        resp.warnings.iter().any(|w| w.contains("input_tokens")),
        "the unreadable field is named: {:?}",
        resp.warnings
    );
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Tokens(t)) => {
            assert_eq!((t.input, t.output), (0, 0), "image tokens never bill");
            assert_eq!(
                t.residual_units,
                std::collections::BTreeMap::from([("image.output_tokens".to_string(), 5)]),
                "the unknown count is left out of the record, never recorded as 0"
            );
        }
        other => panic!("expected a zero token line, got {other:?}"),
    }
    let written: serde_json::Value = serde_json::from_slice(
        &crate::codec::openai_chat::handler::write_image_response(&resp).bytes,
    )
    .unwrap();
    assert_eq!(
        written.pointer("/usage/input_tokens"),
        None,
        "no 0 is written for it: {written}"
    );
    assert_eq!(
        written.pointer("/usage/total_tokens"),
        None,
        "nor a total that would include it"
    );
    assert_eq!(
        written.pointer("/usage/output_tokens"),
        Some(&serde_json::json!(5))
    );

    let ok = br#"{"data":[{"b64_json":"aGk="}],"usage":{"input_tokens":27.0,"output_tokens":5}}"#;
    let resp = crate::codec::openai_chat::handler::read_image_response(ok)
        .expect("a float-spelled count reads");
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Tokens(t)) => {
            assert_eq!((t.input, t.output), (0, 0));
            assert_eq!(t.residual_units.get("image.input_tokens"), Some(&27));
            assert_eq!(t.residual_units.get("image.output_tokens"), Some(&5));
        }
        other => panic!("expected a zero token line, got {other:?}"),
    }
}

/// THE BUFFERED EMBEDDINGS READER REFUSES AN UNREADABLE BILLED COUNT ON ITS OWN (LLM3#6): it bills

#[test]
fn an_unreadable_embeddings_count_refuses_itself_and_the_answer_is_served() {
    let wire = br#"{"model":"m","data":[{"index":0,"embedding":[0.1]}],"usage":{"prompt_tokens":{"n":27}}}"#;
    let resp = crate::codec::openai_chat::handler::read_embeddings_response(wire)
        .expect("an unreadable count refuses itself, never the answer");
    assert_eq!(
        resp.usage.map(|u| u.input),
        Some(0),
        "the refused count bills nothing"
    );
    assert_eq!(resp.embeddings.len(), 1, "the vectors are served");
}

/// THE TRANSCRIPTION READER JUDGES ITS TOKEN COUNTS ONE BY ONE (LLM3#6), as v1.5.5 read them

#[test]
fn an_unreadable_transcription_count_refuses_itself_and_the_transcript_is_served() {
    use busbar_contract::billing::{Billing, TokenUsage};
    let read = |usage: &str| {
        let wire = format!(r#"{{"text":"hi","usage":{{"type":"tokens",{usage}}}}}"#);
        crate::codec::openai_chat::handler::read_transcription_response(wire.as_bytes())
            .expect("an unreadable count refuses itself, never the transcript")
    };
    let r = read(r#""input_tokens":"12","output_tokens":5"#);
    assert_eq!(r.text, "hi");
    assert_eq!(
        r.usage, None,
        "an unreadable input is no token billing, as 1.5.5"
    );
    let r = read(r#""input_tokens":12,"output_tokens":"5""#);
    assert_eq!(r.text, "hi");
    assert_eq!(
        r.usage,
        Some(Billing::Tokens(TokenUsage {
            input: 12,
            output: 0,
            ..Default::default()
        })),
        "the readable input bills, the refused output bills nothing"
    );
}

/// AN ABSENT COUNT IS STILL ZERO, AND A RESPONSE WITH NO USAGE OBJECT STILL READS.

#[test]
fn an_absent_count_still_reads_as_zero_and_no_usage_object_still_reads() {
    let wire = br#"{"data":[{"b64_json":"aGk="}],"usage":{"input_tokens":27}}"#;
    let resp = crate::codec::openai_chat::handler::read_image_response(wire)
        .expect("absent output_tokens reads");
    match resp.billing() {
        Some(busbar_contract::billing::Billing::Tokens(t)) => {
            assert_eq!((t.input, t.output), (0, 0), "image tokens never bill");
            assert_eq!(
                t.residual_units,
                std::collections::BTreeMap::from([("image.input_tokens".to_string(), 27)]),
                "an absent count is zero, and a zero is not recorded"
            );
        }
        other => panic!("expected a zero token line, got {other:?}"),
    }

    let wire = br#"{"data":[{"b64_json":"aGk="}]}"#;
    crate::codec::openai_chat::handler::read_image_response(wire)
        .expect("a usage-less image response reads");
}

/// A WHISPER DURATION READS EXACTLY, AND ECHOES BACK THE BYTES IT ARRIVED AS.

#[test]
fn a_whisper_duration_reads_exactly_and_echoes_the_same_bytes() {
    let wire = br#"{"text":"hi","usage":{"type":"duration","seconds":12.1}}"#;
    let r = crate::codec::openai_chat::handler::read_transcription_response(wire)
        .expect("a duration usage reads");
    match &r.usage {
        Some(busbar_contract::billing::Billing::Duration { seconds }) => {
            assert_eq!(seconds.micros(), 12_100_000, "held as an exact decimal");
            assert_eq!(seconds.to_decimal_string(), "12.1");
        }
        other => panic!("expected a duration, got {other:?}"),
    }

    let wb = crate::codec::leaf_codec::transcription_write_response("openai", &r);
    let v: serde_json::Value = serde_json::from_slice(&wb.bytes).expect("valid json");
    assert_eq!(v["usage"]["type"], "duration");
    assert_eq!(v["usage"]["seconds"], serde_json::json!(12.1));
    assert!(
        String::from_utf8_lossy(&wb.bytes).contains("\"seconds\":12.1"),
        "the wire bytes are unchanged: {}",
        String::from_utf8_lossy(&wb.bytes)
    );
}

/// A DURATION FINER THAN THE SCALE REFUSES RATHER THAN BEING ROUNDED (#81).

#[test]
fn a_duration_finer_than_the_scale_refuses_rather_than_being_rounded() {
    let wire = br#"{"text":"hi","usage":{"type":"duration","seconds":2.7755575615628914}}"#;
    let err = crate::codec::openai_chat::handler::read_transcription_response(wire)
        .expect_err("a seventh decimal place does not fit the scale");
    assert!(
        format!("{err:?}").contains("seconds"),
        "the refusal names the field: {err:?}"
    );
}

/// AN ABSENT OR NULL DURATION IS STILL NOT A BILLING, exactly as before.
#[test]
fn an_absent_duration_is_still_not_a_billing() {
    let wire = br#"{"text":"hi","usage":{"type":"duration"}}"#;
    let r = crate::codec::openai_chat::handler::read_transcription_response(wire).expect("reads");
    assert_eq!(r.usage, None, "no seconds means no duration billing");
}
