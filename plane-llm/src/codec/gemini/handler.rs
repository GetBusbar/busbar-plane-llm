// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Gemini `RequestHandler` + cells. Embeddings via `models/{id}:embedContent`.

use super::{
    COUNT_LABEL, FIELD_ASPECT_RATIO, FIELD_BYTES_BASE64_ENCODED, FIELD_CACHED_CONTENT_TOKEN_COUNT,
    FIELD_CANDIDATES, FIELD_CANDIDATES_TOKEN_COUNT, FIELD_CONTENTS, FIELD_FINISH_REASON,
    FIELD_GENERATE_CONTENT, FIELD_GENERATION_CONFIG, FIELD_GUIDANCE_SCALE, FIELD_INLINE_DATA,
    FIELD_INLINE_DATA_SNAKE, FIELD_MIME_TYPE, FIELD_MIME_TYPE_SNAKE, FIELD_NEGATIVE_PROMPT,
    FIELD_OUTPUT_DIMENSIONALITY, FIELD_PARTS, FIELD_PERSON_GENERATION, FIELD_PREDICTIONS,
    FIELD_PROMPT_TOKEN_COUNT, FIELD_RESPONSE_MODALITIES, FIELD_SAMPLE_COUNT,
    FIELD_SAMPLE_IMAGE_SIZE, FIELD_STREAM_GENERATE_CONTENT, FIELD_THOUGHTS_TOKEN_COUNT,
    FIELD_TOOL_USE_PROMPT_TOKEN_COUNT, FIELD_TOTAL_TOKEN_COUNT, FIELD_USAGE_METADATA, FIELD_VALUES,
    GEMINI_AUDIO, GEMINI_FINISH_STOP,
};
use crate::codec::ir::audio::{SpeechResp, TranscriptionResp};
use crate::codec::ir::embeddings::{
    EmbInput, EmbeddingItem, EmbeddingsReq, EmbeddingsResp, EncFmt, VectorData,
};
use crate::codec::keys;
use crate::codec::leaf_codec::LeafCodec;
use busbar_contract::codec::{CodecError, IngressReject, RequestHandler};

use crate::codec::framework::MediaBlob;
use crate::codec::framework::MediaPayload;
#[cfg(test)]
use busbar_contract::codec::OperationHandler;
use busbar_contract::codec::{EgressCtx, WireBody};
use busbar_contract::media::base64_encode;
use busbar_contract::operation::OpVerb;
use busbar_contract::SlabBytes;
use bytes::Bytes;
use serde_json::{json, Value};

pub struct GeminiRequestHandler;
/// This protocol's own chat instance; remove it and the registry arm and chat 404s via the
static CHAT: super::super::chat_handle::ChatOperation =
    super::super::chat_handle::ChatOperation(COUNT_LABEL);
static EMB: GeminiEmbeddings = GeminiEmbeddings;
static IMG: GeminiImage = GeminiImage;
static TRANSCRIPTION: GeminiTranscription = GeminiTranscription;
static SPEECH: GeminiSpeech = GeminiSpeech;

/// Gemini's row of the support matrix, as data. A verb absent from it is the standard no-handler
static CELLS: &[busbar_contract::codec::Cell] = &[
    (OpVerb::CHAT, &CHAT),
    (OpVerb::EMBEDDINGS, &EMB),
    (OpVerb::IMAGE, &IMG),
    (OpVerb::TRANSCRIPTION, &TRANSCRIPTION),
    (OpVerb::SPEECH, &SPEECH),
];

/// The `:verb` suffix of each verb's egress URL, keyed by this protocol's verb constants. The three
static ACTIONS: &[(OpVerb, &str)] = &[
    (OpVerb::EMBEDDINGS, "embedContent"),
    (OpVerb::IMAGE, "predict"),
];

impl RequestHandler for GeminiRequestHandler {
    dialect_identity!(COUNT_LABEL);
    fn upstream_path(&self, ctx: &EgressCtx) -> String {
        let m = ctx.model;

        let base = ctx.path_base.unwrap_or("/v1beta/models");
        if let Some(action) = crate::codec::framework::path_of(ACTIONS, ctx.operation) {
            return format!("{base}/{m}:{action}");
        }

        let stream_aware =
            [OpVerb::CHAT, OpVerb::TRANSCRIPTION, OpVerb::SPEECH].contains(&ctx.operation);
        let action = if stream_aware && ctx.stream {
            FIELD_STREAM_GENERATE_CONTENT
        } else {
            FIELD_GENERATE_CONTENT
        };
        format!("{base}/{m}:{action}")
    }
    fn resolve_operation(&self, path: &str, body: &[u8]) -> Option<OpVerb> {
        if path.contains(":embedContent") || path.contains(":batchEmbedContents") {
            return Some(OpVerb::EMBEDDINGS);
        }
        if path.contains(":predict") {
            return Some(OpVerb::IMAGE);
        }
        if !(path.contains(":generateContent") || path.contains(":streamGenerateContent")) {
            return None;
        }

        let hit = (0..body.len()).any(|i| {
            let rest = &body[i..];
            rest.starts_with(FIELD_RESPONSE_MODALITIES.as_bytes())
                || rest.starts_with(FIELD_INLINE_DATA_SNAKE.as_bytes())
                || rest.starts_with(FIELD_INLINE_DATA.as_bytes())
        });
        if hit {
            if let Ok(v) = serde_json::from_slice::<Value>(body) {
                let audio_out = v
                    .pointer("/generationConfig/responseModalities")
                    .and_then(Value::as_array)
                    .is_some_and(|m| m.iter().any(|x| x.as_str() == Some(GEMINI_AUDIO)));
                if audio_out {
                    return Some(OpVerb::SPEECH);
                }
                let audio_in = v
                    .pointer("/contents/0/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|p| {
                            p.get(FIELD_INLINE_DATA_SNAKE)
                                .or_else(|| p.get(FIELD_INLINE_DATA))
                                .and_then(|d| {
                                    d.get(FIELD_MIME_TYPE_SNAKE)
                                        .or_else(|| d.get(FIELD_MIME_TYPE))
                                })
                                .and_then(Value::as_str)
                                .is_some_and(|m| m.starts_with("audio/"))
                        })
                    });
                if audio_in {
                    return Some(OpVerb::TRANSCRIPTION);
                }
            }
        }
        Some(OpVerb::CHAT)
    }
    fn path_model(&self, path: &str) -> Option<String> {
        let rest = path.split("/models/").nth(1)?;
        let (model, _action) = rest.rsplit_once(':')?;
        (!model.is_empty()).then(|| model.to_string())
    }
}

/// Gemini transcription: audio understood via `models/{id}:generateContent` with inline audio.
struct GeminiTranscription;

leaf_op! {
    GeminiTranscription: COUNT_LABEL,
    TranscriptionReqHandle = read_transcription_request,
    TranscriptionRespHandle = read_transcription_response;
}

/// The two synthetic directive texts the transcription writer prepends, named so the reader can
const TRANSCRIBE_INSTRUCTION: &str = "Transcribe the following audio verbatim.";
const TRANSLATE_INSTRUCTION: &str = "Translate the following audio to text.";

/// IR to Gemini transcription request wire ([`GeminiTranscription::write_request`]).
pub fn write_transcription_request(r: &crate::codec::ir::audio::TranscriptionReq) -> Bytes {
    let (mime, data) = match &r.audio {
        Some(blob) => {
            let d = match &blob.payload {
                MediaPayload::B64(s) => s.clone(),
                MediaPayload::Bytes(b) => base64_encode(b),
            };
            (blob.mime_type.clone(), d)
        }
        None => (String::new(), String::new()),
    };

    let instruction = if r.target_language.is_some() {
        TRANSLATE_INSTRUCTION
    } else {
        TRANSCRIBE_INSTRUCTION
    };
    let mut parts = vec![json!({ (keys::TEXT): instruction })];

    if let Some(prompt) = &r.prompt {
        if !prompt.is_empty() {
            parts.push(json!({ (keys::TEXT): prompt }));
        }
    }
    parts.push(
        json!({ (FIELD_INLINE_DATA_SNAKE): { (FIELD_MIME_TYPE_SNAKE): mime, (keys::DATA): data } }),
    );
    let mut body =
        json!({ (FIELD_CONTENTS): [{ (keys::ROLE): keys::USER, (FIELD_PARTS): parts }] });

    if let Some(t) = r.temperature {
        body[FIELD_GENERATION_CONFIG] = json!({ "temperature": t });
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Gemini transcription response wire ([`GeminiTranscription::write_response`]).
pub fn write_transcription_response(r: &crate::codec::ir::audio::TranscriptionResp) -> WireBody {
    let mut body = json!({
        (FIELD_CANDIDATES): [{
            (keys::CONTENT): { (FIELD_PARTS): [{ (keys::TEXT): r.text }], (keys::ROLE): keys::MODEL },
            (FIELD_FINISH_REASON): GEMINI_FINISH_STOP,
        }],
    });

    if let Some(busbar_contract::billing::Billing::Tokens(t)) = &r.usage {
        body[FIELD_USAGE_METADATA] = json!({
            (FIELD_PROMPT_TOKEN_COUNT): t.input,
            (FIELD_CANDIDATES_TOKEN_COUNT): t.output,
            (FIELD_TOTAL_TOKEN_COUNT): t.input.saturating_add(t.output),
        });
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Gemini speech (TTS): `models/{id}:generateContent` with `responseModalities: [AUDIO]`. It returns
struct GeminiSpeech;

leaf_op! {
    GeminiSpeech: COUNT_LABEL,
    SpeechReqHandle = read_speech_request,
    SpeechRespHandle = read_speech_response;
}

/// The `text` parts of the parts array at `pointer`, joined (empty when there are none).
fn joined_text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get(keys::TEXT).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// One named prebuilt speaker: `{voiceConfig: {prebuiltVoiceConfig: {voiceName}}}` (the single
fn prebuilt_speaker(name: &str) -> Value {
    json!({ "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": name } } })
}

/// IR to Gemini TTS request wire ([`GeminiSpeech::write_request`]).
pub fn write_speech_request(r: &crate::codec::ir::audio::SpeechReq) -> Bytes {
    let speech_config = if r.speakers.is_empty() {
        prebuilt_speaker(&r.timbre)
    } else {
        let configs: Vec<Value> = r
            .speakers
            .iter()
            .map(|(speaker, timbre)| {
                let mut config = prebuilt_speaker(timbre);
                config[keys::SPEAKER] = json!(speaker);
                config
            })
            .collect();
        json!({ "multiSpeakerVoiceConfig": { "speakerVoiceConfigs": configs } })
    };

    let text = match &r.instructions {
        Some(instr) if !instr.trim().is_empty() => format!("{}: {}", instr.trim(), r.input),
        _ => r.input.clone(),
    };
    let body = json!({
        (FIELD_CONTENTS): [{ (keys::ROLE): keys::USER, (FIELD_PARTS): [{ (keys::TEXT): text }]}],
        (FIELD_GENERATION_CONFIG): { (FIELD_RESPONSE_MODALITIES): [GEMINI_AUDIO], "speechConfig": speech_config },
    });
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Gemini TTS response wire ([`GeminiSpeech::write_response`]).
pub fn write_speech_response(r: &SpeechResp) -> WireBody {
    let (data, mime) = match &r.audio {
        Some(blob) => {
            let d = match &blob.payload {
                MediaPayload::B64(s) => s.clone(),
                MediaPayload::Bytes(b) => base64_encode(b),
            };
            (d, blob.mime_type.clone())
        }
        None => (String::new(), keys::AUDIO_MPEG.into()),
    };
    let body = json!({
        (FIELD_CANDIDATES): [{
            (keys::CONTENT): { (FIELD_PARTS): [{ (FIELD_INLINE_DATA): { (FIELD_MIME_TYPE): mime, (keys::DATA): data } }], (keys::ROLE): keys::MODEL },
            (FIELD_FINISH_REASON): GEMINI_FINISH_STOP,
        }],
    });
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Gemini/Imagen image generation (`models/{id}:predict`): prompt in, `predictions[].bytesBase64Encoded`
struct GeminiImage;

leaf_op! {
    GeminiImage: COUNT_LABEL,
    ImageReqHandle = read_image_request,
    ImageRespHandle = read_image_response;
    // Buffer the same-protocol non-stream 2xx body so the default `extract_usage` runs this op's
    // reader: a token-reporting image model's `usageMetadata` is recorded (never billed; see
    // `ImageResp::billing`) and an Imagen per-image answer bills its `images` count.
    fn taps_usage(&self) -> bool {
        true
    }
}

/// IR to Imagen `:predict` request wire ([`GeminiImage::write_request`]).
pub fn write_image_request(r: &crate::codec::ir::image::ImageReq) -> Bytes {
    let mut params = json!({ (FIELD_SAMPLE_COUNT): r.n.unwrap_or(1) });

    if let Some(a) = &r.aspect_ratio {
        params[FIELD_ASPECT_RATIO] = json!(a);
    }
    if let Some(p) = &r.person_generation {
        params[FIELD_PERSON_GENERATION] = json!(p);
    }

    if let Some(neg) = &r.negative_prompt {
        params[FIELD_NEGATIVE_PROMPT] = json!(neg);
    }
    if let Some(seed) = r.seed {
        params[keys::SEED] = json!(seed);
    }
    if let Some(g) = r.guidance_scale {
        params[FIELD_GUIDANCE_SCALE] = json!(g);
    }
    if let Some(tier) = &r.image_size_tier {
        params[FIELD_SAMPLE_IMAGE_SIZE] = json!(tier);
    }
    let body = json!({
        "instances": [{ "prompt": r.prompt.clone().unwrap_or_default() }],
        (keys::PARAMETERS): params,
    });
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Imagen `:predict` response wire ([`GeminiImage::write_response`]).
pub fn write_image_response(r: &crate::codec::ir::image::ImageResp) -> WireBody {
    let predictions: Vec<Value> = r
        .images
        .iter()
        .map(|img| {
            let mut p = json!({});
            if let Some(b64) = &img.b64 {
                p[FIELD_BYTES_BASE64_ENCODED] = json!(b64);
            }
            p[FIELD_MIME_TYPE] = json!(img.mime_type.clone().unwrap_or_else(|| "image/png".into()));
            p
        })
        .collect();

    let body = json!({ (FIELD_PREDICTIONS): predictions });
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// Gemini embeddings (`models/{id}:embedContent`). Single content in, `embedding.values` out.
struct GeminiEmbeddings;

leaf_op! {
    GeminiEmbeddings: COUNT_LABEL,
    EmbeddingsReqHandle = read_embeddings_request,
    EmbeddingsRespHandle = read_embeddings_response;
    // Token-metered: buffer the same-protocol non-stream 2xx body so the default `extract_usage`
    // reads `usage` and bills the key's TPM/spend.
    fn taps_usage(&self) -> bool {
        true
    }
    fn extract_usage(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _ingress: &str,
        body: &[u8],
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, busbar_contract::codec::UsageTapFault> {
        let value: Value = crate::codec::json::parse(body)
            .map_err(|_| busbar_contract::codec::UsageTapFault::Decode)?;
        Ok(read_embeddings_usage(&value))
    }
    fn recover_response_billing(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        body: &[u8],
    ) -> Option<busbar_contract::billing::Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        read_embeddings_usage(&value).map(busbar_contract::billing::Billing::Tokens)
    }
}

/// IR to Gemini `:embedContent` request wire (the embeddings `OperationHandler::write_request`).
pub fn write_embeddings_request(r: &EmbeddingsReq) -> Bytes {
    let text = match &r.input {
        EmbInput::Text(v) => {
            if v.len() > 1 {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::member(keys::INPUT),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [dropped = v.len() - 1,],
                    "Gemini :embedContent takes one input; embedding only the first of a \
                     multi-input request (the rest are not sent)"
                );
            }
            v.first().cloned().unwrap_or_default()
        }
        other => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::member(keys::INPUT),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [dropped = 1,],
                "Gemini :embedContent takes text input only; dropping a non-text embeddings \
                 input ({other:?} kind) with no analog"
            );
            String::new()
        }
    };

    let mut body = json!({ (keys::CONTENT): { (FIELD_PARTS): [{ (keys::TEXT): text }] } });
    if let Some(d) = r.dimensions {
        body[FIELD_OUTPUT_DIMENSIONALITY] = json!(d);
    }
    if let Some(t) = &r.task_type {
        body[keys::TASK_TYPE] = json!(t);
    }
    if let Some(t) = &r.title {
        body[keys::TITLE] = json!(t);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR to Gemini `:embedContent` response wire (the embeddings `OperationHandler::write_response`).
pub fn write_embeddings_response(r: &EmbeddingsResp) -> WireBody {
    let values: Vec<f32> = r
        .embeddings
        .first()
        .and_then(|item| match item.vectors.get(&EncFmt::Float) {
            Some(VectorData::Float(v)) => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_default();
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&json!({ (keys::EMBEDDING): { (FIELD_VALUES): values } }))
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "tests/handler_tests.rs"]
mod tests;

/// Wire to concrete `TranscriptionReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_transcription_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::audio::TranscriptionReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let mut audio = None;
    let mut prompt = None;
    let mut target_language = None;
    if let Some(parts) = wire.pointer("/contents/0/parts").and_then(Value::as_array) {
        for p in parts {
            let inline = p
                .get(FIELD_INLINE_DATA_SNAKE)
                .or_else(|| p.get(FIELD_INLINE_DATA));
            if let Some(d) = inline {
                let data = d
                    .get(keys::DATA)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if busbar_contract::media::base64_decode(&data).is_none() {
                    return Err(IngressReject::BadRequest(
                        "inline_data.data is not valid base64".into(),
                    ));
                }
                audio = Some(MediaBlob {
                    payload: MediaPayload::B64(data),
                    mime_type: d
                        .get(FIELD_MIME_TYPE_SNAKE)
                        .or_else(|| d.get(FIELD_MIME_TYPE))
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream")
                        .to_string(),
                    pcm: None,
                });
            } else if let Some(t) = p.get(keys::TEXT).and_then(Value::as_str) {
                if t == TRANSLATE_INSTRUCTION {
                    target_language = Some("en".to_string());
                } else if t != TRANSCRIBE_INSTRUCTION {
                    prompt = Some(t.to_string());
                }
            }
        }
    }
    let Some(audio) = audio else {
        return Err(IngressReject::BadRequest(
            "transcription requires an inline_data audio part".into(),
        ));
    };
    let temperature = wire
        .pointer("/generationConfig/temperature")
        .and_then(Value::as_f64)
        .map(|t| t as f32);
    Ok(crate::codec::ir::audio::TranscriptionReq {
        audio: Some(audio),
        prompt,
        target_language,
        temperature,
        ..Default::default()
    })
}

/// `usageMetadata` counts a transcription answer reports beside the two it bills. The chat table
const TRANSCRIPTION_RESIDUAL_COUNTS: [&str; 3] = [
    FIELD_THOUGHTS_TOKEN_COUNT,
    FIELD_CACHED_CONTENT_TOKEN_COUNT,
    FIELD_TOOL_USE_PROMPT_TOKEN_COUNT,
];

/// Residual units of a transcription answer's `usageMetadata` ([`TRANSCRIPTION_RESIDUAL_COUNTS`]),
fn transcription_residual_units(usage: &Value) -> std::collections::BTreeMap<String, u64> {
    let mut warnings = Vec::new();
    TRANSCRIPTION_RESIDUAL_COUNTS
        .iter()
        .filter_map(|&field| {
            crate::codec::usage_count::recorded_count(usage, field, &mut warnings)
                .filter(|n| *n != 0)
                .map(|n| (field.to_string(), n))
        })
        .collect()
}

/// Wire to concrete `TranscriptionResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_transcription_response(
    wire: &[u8],
) -> Result<crate::codec::ir::audio::TranscriptionResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let text = joined_text(&v, "/candidates/0/content/parts");
    let usage = v.get(FIELD_USAGE_METADATA).map(|u| {
        let billed = |field: &'static str| {
            crate::codec::usage_count::billed_count(u, field).unwrap_or_else(|unreadable| {
                crate::codec::usage_count::refused_count(COUNT_LABEL, &unreadable);
                0
            })
        };
        busbar_contract::billing::Billing::Tokens(busbar_contract::billing::TokenUsage {
            input: billed(FIELD_PROMPT_TOKEN_COUNT),
            output: billed(FIELD_CANDIDATES_TOKEN_COUNT),
            residual_units: transcription_residual_units(u),
            ..Default::default()
        })
    });
    Ok(TranscriptionResp {
        text,
        usage,
        ..Default::default()
    })
}

/// Wire to concrete `SpeechReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_speech_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::audio::SpeechReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let input = joined_text(&wire, "/contents/0/parts");
    if input.is_empty() {
        return Err(IngressReject::BadRequest(
            "speech requires a text part".into(),
        ));
    }
    let timbre = wire
        .pointer("/generationConfig/speechConfig/voiceConfig/prebuiltVoiceConfig/voiceName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let speakers = wire
        .pointer("/generationConfig/speechConfig/multiSpeakerVoiceConfig/speakerVoiceConfigs")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let speaker = c.get(keys::SPEAKER).and_then(Value::as_str)?;
                    let timbre_name = c
                        .pointer("/voiceConfig/prebuiltVoiceConfig/voiceName")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some((speaker.to_string(), timbre_name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(crate::codec::ir::audio::SpeechReq {
        input,
        timbre,
        speakers,
        ..Default::default()
    })
}

/// Wire to concrete `SpeechResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_speech_response(
    wire: &[u8],
) -> Result<crate::codec::ir::audio::SpeechResp, CodecError> {
    if let Ok(v) = serde_json::from_slice::<Value>(wire) {
        let Some(data) = v
            .pointer("/candidates/0/content/parts/0/inlineData/data")
            .and_then(Value::as_str)
        else {
            return Err(CodecError::Malformed(
                "gemini speech response carries no candidates[0].content.parts[0].inlineData"
                    .into(),
            ));
        };
        let mime = v
            .pointer("/candidates/0/content/parts/0/inlineData/mimeType")
            .and_then(Value::as_str)
            .unwrap_or("audio/L16;codec=pcm;rate=24000")
            .to_string();
        let pcm = mime
            .contains("pcm")
            .then_some(crate::codec::framework::PcmParams {
                sample_rate: 24000,
                channels: 1,
                bit_depth: 16,
            });

        if busbar_contract::media::base64_decode(data).is_none() {
            return Err(CodecError::Malformed(
                "gemini speech inlineData.data is not valid base64".into(),
            ));
        }
        return Ok(SpeechResp {
            audio: Some(MediaBlob {
                payload: MediaPayload::B64(data.to_string()),
                mime_type: mime,
                pcm,
            }),

            usage: Some(busbar_contract::billing::Billing::Flat),
            ..Default::default()
        });
    }

    Ok(SpeechResp {
        audio: Some(MediaBlob {
            payload: MediaPayload::Bytes(SlabBytes::from(wire)),
            mime_type: keys::AUDIO_MPEG.into(),
            pcm: None,
        }),

        usage: Some(busbar_contract::billing::Billing::Flat),
        ..Default::default()
    })
}

/// Wire to concrete `ImageReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_image_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::image::ImageReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let params = wire.get(keys::PARAMETERS).cloned().unwrap_or_default();
    Ok(crate::codec::ir::image::ImageReq {
        prompt: wire
            .pointer("/instances/0/prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        n: params
            .get(FIELD_SAMPLE_COUNT)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        aspect_ratio: params
            .get(FIELD_ASPECT_RATIO)
            .and_then(Value::as_str)
            .map(str::to_string),
        person_generation: params
            .get(FIELD_PERSON_GENERATION)
            .and_then(Value::as_str)
            .map(str::to_string),

        negative_prompt: params
            .get(FIELD_NEGATIVE_PROMPT)
            .and_then(Value::as_str)
            .map(str::to_string),
        seed: params.get(keys::SEED).and_then(Value::as_u64),
        guidance_scale: params
            .get(FIELD_GUIDANCE_SCALE)
            .and_then(Value::as_f64)
            .map(|f| f as f32),
        image_size_tier: params
            .get(FIELD_SAMPLE_IMAGE_SIZE)
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    })
}

/// Wire to concrete `ImageResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_image_response(wire: &[u8]) -> Result<crate::codec::ir::image::ImageResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let images: Vec<crate::codec::framework::ImageOutput> = v
        .get(FIELD_PREDICTIONS)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|p| crate::codec::framework::ImageOutput {
                    b64: p
                        .get(FIELD_BYTES_BASE64_ENCODED)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    mime_type: p
                        .get(FIELD_MIME_TYPE)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();

    let mut warnings = Vec::new();
    let usage = v
        .get(FIELD_USAGE_METADATA)
        .map(|u| crate::codec::ir::image::ImageTokens {
            input: crate::codec::usage_count::recorded_count(
                u,
                FIELD_PROMPT_TOKEN_COUNT,
                &mut warnings,
            ),
            output: crate::codec::usage_count::recorded_count(
                u,
                FIELD_CANDIDATES_TOKEN_COUNT,
                &mut warnings,
            ),
        });

    let cost_basis = if usage.is_none() {
        Some(crate::codec::ir::image::CostBasis {
            count: u32::try_from(images.len()).unwrap_or(u32::MAX),
            ..Default::default()
        })
    } else {
        None
    };
    Ok(crate::codec::ir::image::ImageResp {
        images,
        usage,
        cost_basis,
        warnings,
        ..Default::default()
    })
}

/// Wire to concrete `EmbeddingsReq`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::embeddings::EmbeddingsReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let text = joined_text(&wire, "/content/parts");
    if text.is_empty() {
        return Err(IngressReject::BadRequest(
            "embedContent requires `content.parts[].text`".into(),
        ));
    }
    Ok(crate::codec::ir::embeddings::EmbeddingsReq {
        input: EmbInput::Text(vec![text]),
        task_type: wire
            .get(keys::TASK_TYPE)
            .and_then(Value::as_str)
            .map(str::to_string),
        title: wire
            .get(keys::TITLE)
            .and_then(Value::as_str)
            .map(str::to_string),
        dimensions: wire
            .get(FIELD_OUTPUT_DIMENSIONALITY)
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok()),
        encoding_formats: vec![EncFmt::Float],
        ..Default::default()
    })
}

/// Wire to concrete `EmbeddingsResp`, shared by the leaf-op handle and the `leaf_codec` read dispatch.
pub fn read_embeddings_response(
    wire: &[u8],
) -> Result<crate::codec::ir::embeddings::EmbeddingsResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let mut item = EmbeddingItem::default();
    let malformed = || CodecError::Malformed("invalid Gemini embedding values".to_string());
    let values = v
        .get(keys::EMBEDDING)
        .and_then(|e| e.get(FIELD_VALUES))
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let coordinates = values
        .iter()
        .map(|value| {
            let coordinate = value.as_f64().ok_or_else(malformed)? as f32;
            coordinate
                .is_finite()
                .then_some(coordinate)
                .ok_or_else(malformed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    item.vectors
        .insert(EncFmt::Float, VectorData::Float(coordinates));
    let usage = read_embeddings_usage(&v);
    Ok(EmbeddingsResp {
        embeddings: vec![item],
        usage,
        ..Default::default()
    })
}

/// Count usage independently of vector decoding so a refused vector still reports provider work.
fn read_embeddings_usage(v: &Value) -> Option<busbar_contract::billing::TokenUsage> {
    crate::codec::usage_count::billed_count_opt(
        v.get(FIELD_USAGE_METADATA),
        FIELD_PROMPT_TOKEN_COUNT,
    )
    .unwrap_or_else(|unreadable| {
        crate::codec::usage_count::refused_count(COUNT_LABEL, &unreadable);
        None
    })
    .map(|n| busbar_contract::billing::TokenUsage {
        input: n,
        ..Default::default()
    })
}

/// This dialect's row of the leaf-op `(operation, protocol)` dispatch, carried on `super::ENTRY`.
pub(crate) const LEAF: crate::codec::leaf_codec::LeafCodecs =
    crate::codec::leaf_codec::LeafCodecs {
        embeddings: Some(LeafCodec {
            write_request: write_embeddings_request,
            write_response: write_embeddings_response,
            read_request: read_embeddings_request,
            read_response: read_embeddings_response,
        }),
        image: Some(LeafCodec {
            write_request: write_image_request,
            write_response: write_image_response,
            read_request: read_image_request,
            read_response: read_image_response,
        }),
        transcription: Some(LeafCodec {
            write_request: write_transcription_request,
            write_response: write_transcription_response,
            read_request: read_transcription_request,
            read_response: read_transcription_response,
        }),
        speech: Some(LeafCodec {
            write_request: write_speech_request,
            write_response: write_speech_response,
            read_request: read_speech_request,
            read_response: read_speech_response,
        }),
        ..crate::codec::leaf_codec::LeafCodecs::NONE
    };
