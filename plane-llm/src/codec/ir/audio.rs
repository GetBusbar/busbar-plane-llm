// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Audio IRs, two opposite operations in one module:

use crate::codec::framework::MediaBlob;
use busbar_contract::billing::Billing;
use busbar_contract::ir::SourceScopedExtra;

/// A word with start/end offsets (seconds).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    pub word: String,
    pub start: f64,
    pub end: f64,
}

/// A transcription segment (verbose_json / diarized).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Segment {
    pub id: i64,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub avg_logprob: Option<f64>,
    pub no_speech_prob: Option<f64>,
    pub compression_ratio: Option<f64>,
    pub speaker: Option<String>,
}

/// Transcription request IR. `audio` is `Option` only so `Default` derives; a real request always
/// carries it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TranscriptionReq {
    pub audio: Option<MediaBlob>,
    pub model: String,
    pub source_language: Option<String>,
    /// `None` = transcribe; `Some("en")` (or other) = TRANSLATE — folds `/audio/translations` in.
    pub target_language: Option<String>,
    pub prompt: Option<String>,
    pub response_format: Option<String>,
    pub temperature: Option<f32>,
    pub stream: bool,
    pub extra: SourceScopedExtra,
}

/// The transcription family's walk, this IR's answer to [`busbar_contract::ir::facts::IrFacts`]. The
impl busbar_contract::ir::facts::IrFacts for TranscriptionReq {
    fn verb(&self) -> busbar_contract::operation::OpVerb {
        busbar_contract::operation::OpVerb::TRANSCRIPTION
    }

    fn wants_stream(&self) -> bool {
        self.stream
    }

    fn end_user(&self) -> Option<&str> {
        None
    }

    fn shape(&self) -> busbar_contract::ir::facts::Shape {
        super::one_turn_shape(self)
    }

    fn content(&self) -> Vec<busbar_contract::ir::facts::ContentItem<'_>> {
        use busbar_contract::ir::facts::{ContentItem, Slot, OPAQUE_CONTENT_MARKER};
        use std::borrow::Cow;
        let mut out = Vec::new();
        if self.audio.is_some() {
            out.push(ContentItem::Opaque {
                author: "user",
                slot: Slot::Turn(0),
                label: "audio",
                marker: OPAQUE_CONTENT_MARKER,
            });
        }
        if let Some(prompt) = &self.prompt {
            out.push(ContentItem::Text {
                author: "user",
                slot: Slot::Turn(0),
                text: Cow::Borrowed(prompt.as_str()),
            });
        }
        out
    }
}

/// Transcription response IR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TranscriptionResp {
    pub text: String,
    pub detected_language: Option<String>,
    pub duration_seconds: Option<f64>,
    pub segments: Vec<Segment>,
    pub words: Vec<Word>,
    /// The response format the body was delivered in, so a writer reproduces the wire shape the caller
    /// asked for. OpenAI serves `text`/`srt`/`vtt` as a raw `text/plain` body, so a reader that parsed
    /// a non-JSON body records it here and the writer re-emits `text/plain` instead of JSON-wrapping.
    /// `None`/`json`/`verbose_json` mean the JSON envelope.
    pub response_format: Option<String>,
    /// `Duration{seconds}` (whisper-1) | `Tokens` (gpt-4o-transcribe) — model-dependent.
    pub usage: Option<Billing>,
    pub extra: SourceScopedExtra,
}

impl TranscriptionResp {
    pub fn billing(&self) -> Option<Billing> {
        self.usage.clone()
    }
}

/// Speech request IR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpeechReq {
    pub input: String,
    pub model: String,
    /// The provider's speaker id: OpenAI's `voice` member, Gemini's `prebuiltVoiceConfig.voiceName`.
    pub timbre: String,
    pub response_format: Option<String>,
    pub speed: Option<f32>,
    pub instructions: Option<String>,
    pub speakers: Vec<(String, String)>,
    pub stream: bool,
    pub extra: SourceScopedExtra,
}

impl SpeechReq {
    /// The TTS request-seam meter. TTS bills by input size (tts-1/-hd: characters), knowable only here:
    pub fn billing(&self) -> Option<Billing> {
        Some(Billing::Characters {
            count: self.input.chars().count() as u64,
        })
    }
}

/// The speech family's walk, this IR's answer to [`busbar_contract::ir::facts::IrFacts`]. Every caller
impl busbar_contract::ir::facts::IrFacts for SpeechReq {
    fn verb(&self) -> busbar_contract::operation::OpVerb {
        busbar_contract::operation::OpVerb::SPEECH
    }

    fn wants_stream(&self) -> bool {
        self.stream
    }

    fn end_user(&self) -> Option<&str> {
        None
    }

    fn shape(&self) -> busbar_contract::ir::facts::Shape {
        super::one_turn_shape(self)
    }

    fn content(&self) -> Vec<busbar_contract::ir::facts::ContentItem<'_>> {
        use busbar_contract::ir::facts::{ContentItem, Slot};
        use std::borrow::Cow;
        let mut out = Vec::new();
        out.push(ContentItem::Text {
            author: "user",
            slot: Slot::Turn(0),
            text: Cow::Borrowed(self.input.as_str()),
        });
        if let Some(instructions) = &self.instructions {
            out.push(ContentItem::Text {
                author: "user",
                slot: Slot::Turn(0),
                text: Cow::Borrowed(instructions.as_str()),
            });
        }
        for (name, _timbre) in &self.speakers {
            out.push(ContentItem::Text {
                author: "user",
                slot: Slot::Turn(0),
                text: Cow::Borrowed(name.as_str()),
            });
        }
        out
    }
}

/// Speech response IR (binary audio out).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpeechResp {
    pub audio: Option<MediaBlob>,
    /// `Characters{count}` (tts-1) | `Tokens` (gpt-4o-mini-tts) — model-dependent.
    pub usage: Option<Billing>,
    pub extra: SourceScopedExtra,
}

impl SpeechResp {
    pub fn billing(&self) -> Option<Billing> {
        self.usage.clone()
    }
}

#[cfg(test)]
#[path = "tests/audio_tests.rs"]
mod tests;
