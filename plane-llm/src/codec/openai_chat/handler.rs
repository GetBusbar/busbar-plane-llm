// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! OpenAI `RequestHandler` and its OperationHandlers. OperationHandlers are pure codecs — wire ↔ IR,
//! both directions, nothing else: moderation, embeddings, images, audio, and chat each get one.

use crate::codec::cells::{CodecError, IngressReject, RequestHandler};
use crate::codec::ir::moderation::{
    ModerationInput, ModerationReq, ModerationResp, ModerationResult,
};
use crate::codec::keys;
use crate::codec::leaf_codec::LeafCodec;

#[cfg(test)]
use crate::codec::cells::OperationHandler;
use crate::codec::cells::{EgressCtx, WireBody};
use busbar_contract::operation::OpVerb;
use busbar_contract::SlabBytes;
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Endpoint paths — each appears on BOTH the egress side (`upstream_path`) and the ingress match
/// (`resolve_operation`); single-sourced so the two sides cannot drift.
const PATH_CHAT_COMPLETIONS: &str = "/v1/chat/completions";
const PATH_EMBEDDINGS: &str = "/v1/embeddings";
const PATH_MODERATIONS: &str = "/v1/moderations";
const PATH_IMAGES_GENERATIONS: &str = "/v1/images/generations";
const PATH_AUDIO_TRANSCRIPTIONS: &str = "/v1/audio/transcriptions";
const PATH_AUDIO_SPEECH: &str = "/v1/audio/speech";
/// Egress-only (OpenAI ships no rerank surface; the arm is unreachable in practice).
const PATH_RERANK: &str = "/v1/rerank";

pub struct OpenAiRequestHandler;
/// This protocol's OWN chat instance — delete this line (and the registry arm) and this
/// protocol's chat 404s via the standard no-handler path; everything else keeps working.
static CHAT: super::super::chat_handle::ChatOperation =
    super::super::chat_handle::ChatOperation(super::VENDOR_NAME);

static MODERATION: OpenAiModeration = OpenAiModeration;
static EMBEDDINGS: OpenAiEmbeddings = OpenAiEmbeddings;
static IMAGE: OpenAiImage = OpenAiImage;
static TRANSCRIPTION: OpenAiTranscription = OpenAiTranscription;
static SPEECH: OpenAiSpeech = OpenAiSpeech;

/// OPENAI'S ROW OF THE SUPPORT MATRIX — the verbs this protocol speaks, as data. A verb absent from
/// this table is the standard no-handler 404, which is what the enumerated `None` arm used to say:
/// OpenAI ships no rerank surface, and the protocol-surface verbs are MCP's and A2A's, so the pair
/// is unrepresentable rather than refused at runtime.
static CELLS: &[crate::codec::cells::Cell] = &[
    (OpVerb::CHAT, &CHAT),
    (OpVerb::MODERATION, &MODERATION),
    (OpVerb::EMBEDDINGS, &EMBEDDINGS),
    (OpVerb::IMAGE, &IMAGE),
    (OpVerb::TRANSCRIPTION, &TRANSCRIPTION),
    (OpVerb::SPEECH, &SPEECH),
];

/// The egress half of the path constants above; `resolve_operation` reads the same constants on the
/// ingress side, so the two directions cannot drift.
static PATHS: &[(OpVerb, &str)] = &[
    (OpVerb::CHAT, PATH_CHAT_COMPLETIONS),
    (OpVerb::EMBEDDINGS, PATH_EMBEDDINGS),
    (OpVerb::MODERATION, PATH_MODERATIONS),
    (OpVerb::IMAGE, PATH_IMAGES_GENERATIONS),
    (OpVerb::TRANSCRIPTION, PATH_AUDIO_TRANSCRIPTIONS),
    (OpVerb::SPEECH, PATH_AUDIO_SPEECH),
];

impl RequestHandler for OpenAiRequestHandler {
    dialect_identity!(super::VENDOR_NAME);
    fn upstream_path(&self, ctx: &EgressCtx) -> String {
        crate::codec::framework::path_of(PATHS, ctx.operation)
            .unwrap_or(PATH_RERANK)
            .into()
    }
    fn resolve_operation(&self, path: &str, _body: &[u8]) -> Option<OpVerb> {
        if path.ends_with(PATH_CHAT_COMPLETIONS) {
            Some(OpVerb::CHAT)
        } else if path.ends_with(PATH_EMBEDDINGS) {
            Some(OpVerb::EMBEDDINGS)
        } else if path.ends_with(PATH_MODERATIONS) {
            Some(OpVerb::MODERATION)
        } else if path.contains("/v1/images/") {
            Some(OpVerb::IMAGE)
        } else if path.ends_with(PATH_AUDIO_TRANSCRIPTIONS)
            || path.ends_with("/v1/audio/translations")
        {
            Some(OpVerb::TRANSCRIPTION)
        } else if path.ends_with(PATH_AUDIO_SPEECH) {
            Some(OpVerb::SPEECH)
        } else {
            None
        }
    }
}

use crate::codec::framework::MediaBlob;
use crate::codec::framework::MediaPayload;
use crate::codec::ir::audio::{SpeechReq, SpeechResp, TranscriptionReq, TranscriptionResp};
use busbar_contract::billing::Billing;
use busbar_contract::media::base64_decode;

/// One decoded part of a `multipart/form-data` body (its value borrowed from the request bytes).
type MultipartField<'a> = crate::exchange::multipart::TranscriptionField<'a>;

/// Sanitize a client-supplied MIME type before it enters the IR: a MIME type is `type/subtype`
fn sanitize_mime_type(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .take_while(|c| !c.is_control())
        .filter(|c| c.is_ascii() && !c.is_control())
        .collect();
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        keys::APPLICATION_OCTET_STREAM.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Decode an audio blob at the egress boundary. A failed decode is an unwritable-body sentinel,
/// never an empty audio part in an otherwise successful form or response.
fn decode_ir_b64(s: &str) -> Option<SlabBytes> {
    let decoded = base64_decode(s);
    if decoded.is_none() {
        tracing::error!(
            "B64 audio payload in the IR failed to decode at egress — a reader's base64 \
             validation invariant was violated; refusing the audio write"
        );
    }
    decoded
}

/// Minimal byte-level `multipart/form-data` parser: enough for the OpenAI audio form (a `model` text
/// part + a binary `file` part).
fn parse_multipart<'a>(body: &'a [u8], content_type: &str) -> Vec<MultipartField<'a>> {
    crate::exchange::multipart::transcription_fields(content_type, body)
}

/// Transcription (STT): multipart audio IN → `{text}` OUT.
struct OpenAiTranscription;

leaf_op! {
    OpenAiTranscription: super::VENDOR_NAME,
    TranscriptionReqHandle = read_transcription_request,
    TranscriptionRespHandle = read_transcription_response;
    // Metered in its own dialect as across one (money = ledger quantity x rate
    // card). A same-dialect transcription's body is buffered so the relay reads the
    // usage the answer reports: whisper's duration as `audio_ms`, a token-metered model's tokens
    // under the token classes, exactly what the translated path already ledgers.
    fn taps_usage(&self) -> bool {
        true
    }
    fn recover_response_billing(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &[u8],
    ) -> Option<Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        parse_transcription_usage(body, value.get(keys::USAGE)?).ok().flatten()
    }
    fn egress_request_content_type(&self) -> &'static str {
        // write_request rebuilds the multipart form around `TRANSCRIPTION_BOUNDARY`.
        TRANSCRIPTION_CONTENT_TYPE
    }
}

/// The multipart boundary the transcription egress form is framed with: a constant, so the
/// plugin keeps no process-global state for it (a plugin holds no process-global state) and the
/// upstream receives the same bytes each time.
pub const TRANSCRIPTION_BOUNDARY: &str = "----busbaraudioMIME";

/// The Content-Type the transcription egress form is declared with; names [`TRANSCRIPTION_BOUNDARY`].
const TRANSCRIPTION_CONTENT_TYPE: &str = "multipart/form-data; boundary=----busbaraudioMIME";

/// Does `part` contain the multipart delimiter, i.e. can splicing it into a body framed with
/// `boundary` terminate its own part and start a new one?
fn part_carries_delimiter(part: &[u8], boundary: &str) -> bool {
    let head = format!("--{boundary}");
    let head = head.as_bytes();
    if part.starts_with(head) {
        return true;
    }
    let needle = [b"\n".as_slice(), head].concat();
    part.windows(needle.len()).any(|w| w == needle)
}

/// IR → openai multipart transcription request wire (the body of
/// [`OpenAiTranscription::write_request`], moved behind the `(transcription, openai)` key).
pub fn write_transcription_request(r: &TranscriptionReq) -> Bytes {
    let boundary = TRANSCRIPTION_BOUNDARY;
    if [
        Some(r.model.as_str()),
        r.source_language.as_deref(),
        r.prompt.as_deref(),
        r.response_format.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| part_carries_delimiter(value.as_bytes(), boundary))
    {
        tracing::warn!(
            "openai transcription egress: text field carries multipart delimiter; refusing request"
        );
        return Bytes::new();
    }
    let mut out: Vec<u8> = Vec::new();
    let mut push_field = |name: &str, val: &str| {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n"
            )
            .as_bytes(),
        );
    };
    push_field(keys::MODEL, &r.model);

    if let Some(lang) = &r.source_language {
        push_field(super::LANGUAGE, lang);
    }
    if let Some(prompt) = &r.prompt {
        push_field(keys::PROMPT, prompt);
    }
    if let Some(fmt) = &r.response_format {
        push_field(keys::RESPONSE_FORMAT, fmt);
    }

    if let Some(t) = r.temperature {
        push_field(keys::TEMPERATURE, &t.to_string());
    }
    if let Some(blob) = &r.audio {
        let bytes = match &blob.payload {
            MediaPayload::Bytes(b) => b.clone(),
            MediaPayload::B64(s) => {
                let Some(decoded) = decode_ir_b64(s) else {
                    return Bytes::new();
                };
                decoded
            }
        };

        let safe_mime = sanitize_mime_type(&blob.mime_type);

        if part_carries_delimiter(&bytes, boundary) {
            tracing::warn!(
                "openai transcription egress: audio part carries the multipart delimiter; \
                 refusing the request rather than emitting a body it could inject parts into"
            );
            return Bytes::new();
        }
        out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio\"\r\nContent-Type: {safe_mime}\r\n\r\n").as_bytes());
        out.extend_from_slice(&bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Bytes::from(out)
}

/// IR → openai transcription response wire (the body of [`OpenAiTranscription::write_response`], moved
/// behind the `(transcription, openai)` key).
pub fn write_transcription_response(r: &TranscriptionResp) -> WireBody {
    if matches!(
        r.response_format.as_deref(),
        Some(keys::TEXT | "srt" | "vtt")
    ) {
        return WireBody::typed(SlabBytes::from(r.text.clone().into_bytes()), "text/plain");
    }
    let mut body = json!({ (keys::TEXT): r.text });

    if let Some(lang) = &r.detected_language {
        body[super::LANGUAGE] = json!(lang);
    }
    if let Some(d) = r.duration_seconds {
        body[super::W_DURATION] = json!(d);
    }
    if !r.segments.is_empty() {
        body[super::SEGMENTS] = Value::Array(
            r.segments
                .iter()
                .map(|s| {
                    let mut o = json!({
                        (keys::ID): s.id, (keys::START): s.start, (keys::END): s.end, (keys::TEXT): s.text,
                    });
                    if let Some(v) = s.avg_logprob {
                        o[super::AVG_LOGPROB] = json!(v);
                    }
                    if let Some(v) = s.no_speech_prob {
                        o[super::NO_SPEECH_PROB] = json!(v);
                    }
                    if let Some(v) = s.compression_ratio {
                        o[super::COMPRESSION_RATIO] = json!(v);
                    }
                    if let Some(sp) = &s.speaker {
                        o[keys::SPEAKER] = json!(sp);
                    }
                    o
                })
                .collect(),
        );
    }
    if !r.words.is_empty() {
        body[super::W_WORDS] = Value::Array(
            r.words
                .iter()
                .map(|w| json!({ (super::WORD): w.word, (keys::START): w.start, (keys::END): w.end }))
                .collect(),
        );
    }

    match &r.usage {
        Some(Billing::Duration { seconds }) => {
            let seconds = crate::codec::framework::duration_seconds_to_wire(*seconds);
            body[keys::USAGE] =
                json!({ (keys::TYPE): super::W_DURATION, (super::SECONDS): seconds });
        }
        Some(Billing::Tokens(t)) => {
            body[keys::USAGE] = json!({ (keys::TYPE): "tokens", (keys::INPUT_TOKENS): t.input,
                (keys::OUTPUT_TOKENS): t.output, (keys::TOTAL_TOKENS): t.input.saturating_add(t.output) });
        }
        _ => {}
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

/// OpenAI transcription `usage` → `Billing`: `{type:"duration",seconds}` (whisper) or a token shape.
fn parse_transcription_usage(wire: &[u8], u: &Value) -> Result<Option<Billing>, CodecError> {
    match u.get(keys::TYPE).and_then(Value::as_str) {
        Some(super::W_DURATION) => {
            if u.get(super::SECONDS).is_none() {
                return Ok(None);
            }
            let seconds = busbar_contract::Count::read_at(wire, "/usage/seconds")
                .map_err(|e| CodecError::Malformed(format!("usage.seconds: {e}")))?
                .ok_or_else(|| {
                    CodecError::Malformed("usage.seconds: located then lost".to_string())
                })?;
            Ok(Some(Billing::Duration { seconds }))
        }

        _ => match crate::codec::usage_count::billed_count_opt(Some(u), keys::INPUT_TOKENS) {
            Ok(None) => Ok(None),
            Err(unreadable) => {
                crate::codec::usage_count::refused_count(super::VENDOR_NAME, &unreadable);
                Ok(None)
            }
            Ok(Some(input)) => Ok(Some(Billing::Tokens(
                busbar_contract::billing::TokenUsage {
                    input,
                    output: crate::codec::usage_count::billed_count(u, keys::OUTPUT_TOKENS)
                        .unwrap_or_else(|unreadable| {
                            crate::codec::usage_count::refused_count(
                                super::VENDOR_NAME,
                                &unreadable,
                            );
                            0
                        }),
                    ..Default::default()
                },
            ))),
        },
    }
}

/// Speech (TTS): `{input}` IN → binary audio OUT.
struct OpenAiSpeech;

leaf_op! {
    OpenAiSpeech: super::VENDOR_NAME,
    SpeechReqHandle = read_speech_request,
    SpeechRespHandle = read_speech_response;
}

/// IR → openai speech (TTS) request wire (the body of [`OpenAiSpeech::write_request`], moved behind
/// the `(speech, openai)` key).
pub fn write_speech_request(r: &SpeechReq) -> Bytes {
    let mut body = json!({ (keys::MODEL): r.model, (keys::INPUT): r.input, "voice": r.timbre });
    if let Some(f) = &r.response_format {
        body[keys::RESPONSE_FORMAT] = json!(f);
    }

    if let Some(instr) = &r.instructions {
        body[keys::INSTRUCTIONS] = json!(instr);
    }
    if let Some(speed) = r.speed {
        body[super::SPEED] = json!(speed);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR → openai speech (TTS) response wire (the body of [`OpenAiSpeech::write_response`], moved behind
/// the `(speech, openai)` key): raw audio bytes with the blob's own content-type.
pub fn write_speech_response(r: &SpeechResp) -> WireBody {
    let Some(blob) = &r.audio else {
        return WireBody::json(SlabBytes::default());
    };
    let bytes = match &blob.payload {
        MediaPayload::Bytes(b) => b.clone(),
        MediaPayload::B64(s) => {
            let Some(decoded) = decode_ir_b64(s) else {
                return WireBody::json(SlabBytes::default());
            };
            decoded
        }
    };
    WireBody::typed(bytes, &sanitize_mime_type(&blob.mime_type))
}

use crate::codec::ir::embeddings::{
    EmbInput, EmbeddingItem, EmbeddingsReq, EmbeddingsResp, EncFmt, VectorData,
};

/// openai embeddings wire → IR (used when openai is the INGRESS of a cross-protocol call).
struct OpenAiEmbeddings;

leaf_op! {
    OpenAiEmbeddings: super::VENDOR_NAME,
    EmbeddingsReqHandle = read_embeddings_request,
    EmbeddingsRespHandle = read_embeddings_response;
    // Token-metered: buffer the same-protocol non-stream 2xx body so the default
    // `extract_usage` can read the `usage` object and bill the virtual key's TPM/spend
    // (the cross-protocol path already bills; this closes the same-protocol gap).
    fn taps_usage(&self) -> bool {
        true
    }
    fn extract_usage(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        _ingress: &str,
        body: &[u8],
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, crate::codec::cells::UsageTapFault> {
        let value: Value = crate::codec::json::parse(body)
            .map_err(|_| crate::codec::cells::UsageTapFault::Decode)?;
        Ok(read_embeddings_usage(&value))
    }
    fn recover_response_billing(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &[u8],
    ) -> Option<busbar_contract::billing::Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        read_embeddings_usage(&value).map(busbar_contract::billing::Billing::Tokens)
    }

}

/// IR → openai embeddings request wire (the body of [`OpenAiEmbeddings::write_request`], moved behind
/// the `(embeddings, openai)` key).
pub fn write_embeddings_request(r: &EmbeddingsReq) -> Bytes {
    let input = match &r.input {
        EmbInput::Text(v) if v.len() == 1 => json!(v[0]),
        EmbInput::Text(v) => json!(v),
        other => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::member(keys::INPUT),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [dropped = 1,],
                "openai embeddings input is text-only here; dropping a non-text embeddings \
                 input ({other:?} kind) with no analog"
            );
            json!([])
        }
    };
    let mut body = json!({ (keys::MODEL): r.model, (keys::INPUT): input });
    if let Some(d) = r.dimensions {
        body[keys::DIMENSIONS] = json!(d);
    }

    if let Some(u) = &r.user {
        body[keys::USER] = json!(u);
    }

    if r.encoding_formats.contains(&EncFmt::Base64) {
        body[super::ENCODING_FORMAT] = json!(keys::BASE64);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR → openai embeddings response wire (the body of [`OpenAiEmbeddings::write_response`], moved
/// behind the `(embeddings, openai)` key).
pub fn write_embeddings_response(r: &EmbeddingsResp) -> WireBody {
    let data: Vec<Value> = r
        .embeddings
        .iter()
        .map(|item| {
            let emb = match item.vectors.get(&EncFmt::Float) {
                Some(VectorData::Float(f)) => json!(f),
                _ => match item.vectors.values().next() {
                    Some(VectorData::Base64(b)) => json!(b),
                    Some(VectorData::Int(v)) => json!(v),
                    _ => json!([]),
                },
            };
            json!({ (keys::OBJECT): keys::EMBEDDING, (keys::INDEX): item.index, (keys::EMBEDDING): emb })
        })
        .collect();
    let mut body = json!({ (keys::OBJECT): super::LIST, (keys::DATA): data });
    if let Some(m) = &r.model {
        body[keys::MODEL] = json!(m);
    }
    if let Some(u) = &r.usage {
        body[keys::USAGE] = json!({ (super::PROMPT_TOKENS): u.input, (keys::TOTAL_TOKENS): u.input.saturating_add(u.output) });
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

use crate::codec::framework::ImageOutput;
use crate::codec::ir::image::{ImageOp, ImageReq, ImageResp, ImageSize};

struct OpenAiImage;

leaf_op! {
    OpenAiImage: super::VENDOR_NAME,
    ImageReqHandle = read_image_request,
    ImageRespHandle = read_image_response;
    // Buffer the same-protocol non-stream 2xx body so the default `extract_usage` runs this op's
    // reader: a gpt-image-1 answer's reported tokens are recorded (never billed; see
    // `ImageResp::billing`), and a per-image answer with no `usage` bills its `images` count.
    fn taps_usage(&self) -> bool {
        true
    }
    fn recover_response_billing(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &[u8],
    ) -> Option<Billing> {
        let value: Value = crate::codec::json::parse(body).ok()?;
        let usage = read_image_usage(&value, &mut Vec::new())?;
        crate::codec::ir::image::ImageResp { usage: Some(usage), ..Default::default() }.billing()
    }

}

/// IR → openai image request wire (the body of [`OpenAiImage::write_request`], moved behind the
/// `(image, openai)` key).
pub fn write_image_request(r: &ImageReq) -> Bytes {
    let mut body = json!({ (keys::MODEL): r.model });
    if let Some(p) = &r.prompt {
        body[keys::PROMPT] = json!(p);
    }
    if let Some(n) = r.n {
        body[super::W_N] = json!(n);
    }
    match r.size {
        Some(ImageSize::Wh { width, height }) => {
            body[super::SIZE] = json!(format!("{width}x{height}"));
        }
        Some(ImageSize::Auto) => body[super::SIZE] = json!(keys::AUTO),
        None => {}
    }

    if let Some(q) = &r.quality {
        body[keys::QUALITY] = json!(q);
    }
    if let Some(s) = &r.style {
        body[super::STYLE] = json!(s);
    }
    if let Some(f) = &r.response_format {
        body[keys::RESPONSE_FORMAT] = json!(f);
    }

    if let Some(b) = &r.background {
        body[super::BACKGROUND] = json!(b);
    }
    if let Some(f) = &r.output_format {
        body[keys::OUTPUT_FORMAT] = json!(f);
    }
    if let Some(c) = r.output_compression {
        body[super::OUTPUT_COMPRESSION] = json!(c);
    }
    if let Some(m) = &r.moderation {
        body[keys::MODERATION] = json!(m);
    }
    if let Some(u) = &r.user {
        body[keys::USER] = json!(u);
    }
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR → openai image response wire (the body of [`OpenAiImage::write_response`], moved behind the
/// `(image, openai)` key).
pub fn write_image_response(r: &ImageResp) -> WireBody {
    let data: Vec<Value> = r
        .images
        .iter()
        .map(|img| {
            let mut o = serde_json::Map::new();
            if let Some(b) = &img.b64 {
                o.insert(super::B64_JSON.into(), json!(b));
            }
            if let Some(u) = &img.url {
                o.insert(keys::URL.into(), json!(u));
            }
            if let Some(rp) = &img.revised_prompt {
                o.insert(super::REVISED_PROMPT.into(), json!(rp));
            }
            Value::Object(o)
        })
        .collect();
    let mut body = json!({ (keys::DATA): data });

    if let Some(created) = r.created {
        body[super::CREATED] = json!(created);
    }

    if let Some(u) = &r.usage {
        let mut usage = serde_json::Map::new();
        if let Some(n) = u.input {
            usage.insert(keys::INPUT_TOKENS.into(), json!(n));
        }
        if let Some(n) = u.output {
            usage.insert(keys::OUTPUT_TOKENS.into(), json!(n));
        }
        if let (Some(i), Some(o)) = (u.input, u.output) {
            usage.insert(keys::TOTAL_TOKENS.into(), json!(i.saturating_add(o)));
        }
        body[keys::USAGE] = Value::Object(usage);
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

struct OpenAiModeration;

leaf_op! {
    OpenAiModeration: super::VENDOR_NAME,
    ModerationReqHandle = read_moderation_request,
    ModerationRespHandle = read_moderation_response;
}

/// IR → openai moderation request wire (the body of [`OpenAiModeration::write_request`], moved behind
/// the `(moderation, openai)` key).
pub fn write_moderation_request(r: &ModerationReq) -> Bytes {
    let body = json!({ (keys::MODEL): r.model, (keys::INPUT): input_to_value(&r.input) });
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// IR → openai moderation response wire (the body of [`OpenAiModeration::write_response`], moved
/// behind the `(moderation, openai)` key).
pub fn write_moderation_response(r: &ModerationResp) -> WireBody {
    let results: Vec<Value> = r
        .results
        .iter()
        .map(|res| {
            json!({
                (super::FLAGGED): res.flagged,
                (super::CATEGORIES): json_map(&res.categories),
                (super::CATEGORY_SCORES): json_map(&res.category_scores),
                (super::CATEGORY_APPLIED_INPUT_TYPES): json_map(&res.applied_input_types),
            })
        })
        .collect();
    let mut body = json!({ (keys::RESULTS): results });
    if let Some(id) = &r.id {
        body[keys::ID] = json!(id);
    }
    if let Some(m) = &r.model {
        body[keys::MODEL] = json!(m);
    }
    WireBody::json(SlabBytes::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ))
}

fn parse_input(v: Option<&Value>) -> Result<Vec<ModerationInput>, IngressReject> {
    match v {
        Some(Value::String(s)) => Ok(vec![ModerationInput::Text(s.clone())]),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(ModerationInput::Text(s.clone())),
                Value::Object(o) => match o.get(keys::TYPE).and_then(Value::as_str) {
                    Some(keys::IMAGE_URL) => o
                        .get(keys::IMAGE_URL)
                        .and_then(|iu| iu.get(keys::URL))
                        .and_then(Value::as_str)
                        .map(|u| ModerationInput::ImageUrl(u.to_string()))
                        .ok_or_else(|| IngressReject::BadRequest("image_url missing url".into())),
                    _ => o
                        .get(keys::TEXT)
                        .and_then(Value::as_str)
                        .map(|t| ModerationInput::Text(t.to_string()))
                        .ok_or_else(|| IngressReject::BadRequest("input item missing text".into())),
                },
                _ => Err(IngressReject::BadRequest("invalid input item".into())),
            })
            .collect(),
        _ => Err(IngressReject::BadRequest(
            "moderation request missing `input`".into(),
        )),
    }
}

fn input_to_value(input: &[ModerationInput]) -> Value {
    if let [ModerationInput::Text(s)] = input {
        return json!(s);
    }
    Value::Array(
        input
            .iter()
            .map(|i| match i {
                ModerationInput::Text(t) => json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): t }),
                ModerationInput::ImageUrl(u) => {
                    json!({ (keys::TYPE): keys::IMAGE_URL, (keys::IMAGE_URL): { (keys::URL): u } })
                }
            })
            .collect(),
    )
}

fn parse_result(v: &Value) -> Result<ModerationResult, CodecError> {
    Ok(ModerationResult {
        flagged: v
            .get(super::FLAGGED)
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                CodecError::Malformed("moderation result requires a boolean flagged verdict".into())
            })?,
        categories: obj_map(v.get(super::CATEGORIES), |x| x.as_bool()),
        category_scores: obj_map(v.get(super::CATEGORY_SCORES), |x| x.as_f64()),
        applied_input_types: obj_map(v.get(super::CATEGORY_APPLIED_INPUT_TYPES), |x| {
            x.as_array().map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
        }),
    })
}

fn obj_map<T>(v: Option<&Value>, f: impl Fn(&Value) -> Option<T>) -> BTreeMap<String, T> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| f(val).map(|t| (k.clone(), t)))
                .collect()
        })
        .unwrap_or_default()
}

fn json_map<V: serde::Serialize>(m: &BTreeMap<String, V>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

#[cfg(test)]
#[path = "tests/handler_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/speech_mime_regression_tests.rs"]
mod speech_mime_regression_tests;

/// Wire -> concrete `TranscriptionReq` parse, extracted from the `OperationHandler::read_request`
pub fn read_transcription_request(
    body: &[u8],
    content_type: &str,
) -> Result<crate::codec::ir::audio::TranscriptionReq, IngressReject> {
    let fields = parse_multipart(body, content_type);
    let mut req = TranscriptionReq::default();
    for f in &fields {
        match f.name.as_str() {
            keys::MODEL => req.model = String::from_utf8_lossy(f.value).trim().to_string(),
            super::LANGUAGE => {
                req.source_language = Some(String::from_utf8_lossy(f.value).trim().to_string())
            }
            keys::PROMPT => req.prompt = Some(String::from_utf8_lossy(f.value).into_owned()),
            keys::RESPONSE_FORMAT => {
                req.response_format = Some(String::from_utf8_lossy(f.value).trim().to_string())
            }
            keys::TEMPERATURE => {
                req.temperature = String::from_utf8_lossy(f.value).trim().parse().ok()
            }
            super::FILE => {
                req.audio = Some(MediaBlob {
                    payload: MediaPayload::Bytes(SlabBytes::from(f.value)),
                    mime_type: f
                        .content_type
                        .as_deref()
                        .map(sanitize_mime_type)
                        .unwrap_or_else(|| keys::APPLICATION_OCTET_STREAM.into()),
                    pcm: None,
                })
            }
            _ => {}
        }
    }
    if req.audio.is_none() {
        return Err(IngressReject::BadRequest(
            "transcription requires a file part".into(),
        ));
    }
    Ok(req)
}

/// Wire -> concrete `TranscriptionResp` parse, extracted from the `OperationHandler::read_response`
pub fn read_transcription_response(
    wire: &[u8],
) -> Result<crate::codec::ir::audio::TranscriptionResp, CodecError> {
    let v = match serde_json::from_slice::<Value>(wire) {
        Ok(v) if v.get(keys::TEXT).is_some_and(Value::is_string) => v,

        _ => {
            return Ok(TranscriptionResp {
                text: String::from_utf8_lossy(wire).into_owned(),
                response_format: Some(keys::TEXT.to_string()),
                ..Default::default()
            });
        }
    };
    let text = v
        .get(keys::TEXT)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let segments = v
        .get(super::SEGMENTS)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|s| crate::codec::ir::audio::Segment {
                    id: s.get(keys::ID).and_then(Value::as_i64).unwrap_or(0),
                    start: s.get(keys::START).and_then(Value::as_f64).unwrap_or(0.0),
                    end: s.get(keys::END).and_then(Value::as_f64).unwrap_or(0.0),
                    text: s
                        .get(keys::TEXT)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    avg_logprob: s.get(super::AVG_LOGPROB).and_then(Value::as_f64),
                    no_speech_prob: s.get(super::NO_SPEECH_PROB).and_then(Value::as_f64),
                    compression_ratio: s.get(super::COMPRESSION_RATIO).and_then(Value::as_f64),
                    speaker: s
                        .get(keys::SPEAKER)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    let words = v
        .get(super::W_WORDS)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|w| crate::codec::ir::audio::Word {
                    word: w
                        .get(super::WORD)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    start: w.get(keys::START).and_then(Value::as_f64).unwrap_or(0.0),
                    end: w.get(keys::END).and_then(Value::as_f64).unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(TranscriptionResp {
        text,
        detected_language: v
            .get(super::LANGUAGE)
            .and_then(Value::as_str)
            .map(str::to_string),
        duration_seconds: v.get(super::W_DURATION).and_then(Value::as_f64),
        segments,
        words,
        usage: match v.get(keys::USAGE) {
            Some(u) => parse_transcription_usage(wire, u)?,
            None => None,
        },
        ..Default::default()
    })
}

/// Wire -> concrete `SpeechReq` parse, extracted from the `OperationHandler::read_request`
pub fn read_speech_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::audio::SpeechReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let get = |k: &str| {
        wire.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(SpeechReq {
        input: get(keys::INPUT),
        model: get(keys::MODEL),
        timbre: get("voice"),
        response_format: wire
            .get(keys::RESPONSE_FORMAT)
            .and_then(Value::as_str)
            .map(str::to_string),
        instructions: wire
            .get(keys::INSTRUCTIONS)
            .and_then(Value::as_str)
            .map(str::to_string),
        speed: wire
            .get(super::SPEED)
            .and_then(Value::as_f64)
            .map(|s| s as f32),
        ..Default::default()
    })
}

/// Wire -> concrete `SpeechResp` parse, extracted from the `OperationHandler::read_response`
pub fn read_speech_response(
    wire: &[u8],
) -> Result<crate::codec::ir::audio::SpeechResp, CodecError> {
    Ok(SpeechResp {
        audio: Some(MediaBlob {
            payload: MediaPayload::Bytes(SlabBytes::from(wire)),
            mime_type: sniff_speech_audio_mime(wire).into(),
            pcm: None,
        }),

        usage: Some(Billing::Flat),
        ..Default::default()
    })
}

/// Recover the audio Content-Type of an OpenAI speech (TTS) response body from its container magic
/// bytes, mapping onto the mime for each `response_format` the endpoint can emit.
fn sniff_speech_audio_mime(wire: &[u8]) -> &'static str {
    if wire.starts_with(b"ID3") {
        keys::AUDIO_MPEG
    } else if wire.starts_with(b"OggS") {
        "audio/opus"
    } else if wire.starts_with(b"fLaC") {
        "audio/flac"
    } else if wire.len() >= 12 && wire[0..4] == *b"RIFF" && wire[8..12] == *b"WAVE" {
        super::AUDIO_WAV
    } else if wire.len() >= 2 && wire[0] == 0xFF && (wire[1] == 0xF1 || wire[1] == 0xF9) {
        "audio/aac"
    } else {
        keys::AUDIO_MPEG
    }
}

/// Wire -> concrete `EmbeddingsReq` parse, extracted from the `OperationHandler::read_request`
pub fn read_embeddings_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::embeddings::EmbeddingsReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let model = wire
        .get(keys::MODEL)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input = match wire.get(keys::INPUT) {
        Some(Value::String(s)) => EmbInput::Text(vec![s.clone()]),
        Some(Value::Array(a)) => {
            if a.is_empty() || !a.iter().all(Value::is_string) {
                return Err(IngressReject::BadRequest(
                    "embeddings `input` must be a string or a non-empty array of strings \
                         (pre-tokenized integer input is not supported)"
                        .into(),
                ));
            }
            EmbInput::Text(
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
            )
        }
        _ => {
            return Err(IngressReject::BadRequest(
                "embeddings request missing `input`".into(),
            ));
        }
    };
    let dimensions = wire
        .get(keys::DIMENSIONS)
        .and_then(Value::as_u64)
        .and_then(|d| u32::try_from(d).ok());
    let encoding_formats = match wire.get(super::ENCODING_FORMAT).and_then(Value::as_str) {
        Some(keys::BASE64) => vec![EncFmt::Base64],
        _ => vec![EncFmt::Float],
    };
    Ok(EmbeddingsReq {
        model,
        input,
        dimensions,
        encoding_formats,
        user: wire
            .get(keys::USER)
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    })
}

/// Wire -> concrete `EmbeddingsResp` parse, extracted from the `OperationHandler::read_response`
pub fn read_embeddings_response(
    wire: &[u8],
) -> Result<crate::codec::ir::embeddings::EmbeddingsResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let embeddings = v
        .get(keys::DATA)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CodecError::Malformed("OpenAI embeddings response missing data array".into())
        })?
        .iter()
        .enumerate()
        .map(|(idx, d)| {
            let index = d
                .get(keys::INDEX)
                .and_then(Value::as_u64)
                .unwrap_or(idx as u64) as usize;
            let mut item = EmbeddingItem {
                index,
                ..Default::default()
            };
            if let Some(f) = d.get(keys::EMBEDDING).and_then(Value::as_array) {
                let malformed =
                    || CodecError::Malformed("invalid OpenAI embedding component".into());
                let coordinates = f
                    .iter()
                    .map(|value| {
                        let number = value.as_f64().ok_or_else(malformed)? as f32;
                        number.is_finite().then_some(number).ok_or_else(malformed)
                    })
                    .collect::<Result<Vec<_>, CodecError>>()?;
                item.vectors
                    .insert(EncFmt::Float, VectorData::Float(coordinates));
            } else if let Some(b) = d.get(keys::EMBEDDING).and_then(Value::as_str) {
                item.vectors
                    .insert(EncFmt::Base64, VectorData::Base64(b.to_string()));
            } else {
                return Err(CodecError::Malformed(
                    "OpenAI embedding must be an array or base64 string".into(),
                ));
            }
            Ok(item)
        })
        .collect::<Result<Vec<_>, CodecError>>()?;

    let usage = read_embeddings_usage(&v);
    Ok(EmbeddingsResp {
        model: v
            .get(keys::MODEL)
            .and_then(Value::as_str)
            .map(str::to_string),
        object_kind: Some(super::LIST.into()),
        embeddings,
        usage,
        ..Default::default()
    })
}

fn read_embeddings_usage(v: &Value) -> Option<busbar_contract::billing::TokenUsage> {
    v.get(keys::USAGE)
        .map(|u| busbar_contract::billing::TokenUsage {
            input: crate::codec::usage_count::billed_count(u, super::PROMPT_TOKENS).unwrap_or_else(
                |unreadable| {
                    crate::codec::usage_count::refused_count(super::VENDOR_NAME, &unreadable);
                    0
                },
            ),
            ..Default::default()
        })
}

/// Wire -> concrete `ImageReq` parse, extracted from the `OperationHandler::read_request`
pub fn read_image_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::image::ImageReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let model = wire
        .get(keys::MODEL)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if wire.get("image").is_some() {
        return Err(IngressReject::UnsupportedSubOp {
            op: OpVerb::IMAGE,
            model,
        });
    }
    let size = wire.get(super::SIZE).and_then(Value::as_str).and_then(|s| {
        if s == keys::AUTO {
            Some(ImageSize::Auto)
        } else {
            s.split_once('x').and_then(|(w, h)| {
                Some(ImageSize::Wh {
                    width: w.parse().ok()?,
                    height: h.parse().ok()?,
                })
            })
        }
    });
    Ok(ImageReq {
        op: ImageOp::Generate,
        model,
        prompt: wire
            .get(keys::PROMPT)
            .and_then(Value::as_str)
            .map(str::to_string),
        n: wire
            .get(super::W_N)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        size,
        quality: wire
            .get(keys::QUALITY)
            .and_then(Value::as_str)
            .map(str::to_string),
        style: wire
            .get(super::STYLE)
            .and_then(Value::as_str)
            .map(str::to_string),
        response_format: wire
            .get(keys::RESPONSE_FORMAT)
            .and_then(Value::as_str)
            .map(str::to_string),

        background: wire
            .get(super::BACKGROUND)
            .and_then(Value::as_str)
            .map(str::to_string),
        output_format: wire
            .get(keys::OUTPUT_FORMAT)
            .and_then(Value::as_str)
            .map(str::to_string),
        output_compression: wire
            .get(super::OUTPUT_COMPRESSION)
            .and_then(Value::as_u64)
            .and_then(|c| u8::try_from(c).ok()),
        moderation: wire
            .get(keys::MODERATION)
            .and_then(Value::as_str)
            .map(str::to_string),
        user: wire
            .get(keys::USER)
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    })
}

fn read_image_usage(
    v: &Value,
    warnings: &mut Vec<String>,
) -> Option<crate::codec::ir::image::ImageTokens> {
    v.get(keys::USAGE)
        .map(|u| crate::codec::ir::image::ImageTokens {
            input: crate::codec::usage_count::recorded_count(u, keys::INPUT_TOKENS, warnings),
            output: crate::codec::usage_count::recorded_count(u, keys::OUTPUT_TOKENS, warnings),
        })
}

/// Wire -> concrete `ImageResp` parse, extracted from the `OperationHandler::read_response`
pub fn read_image_response(wire: &[u8]) -> Result<crate::codec::ir::image::ImageResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let images: Vec<ImageOutput> = v
        .get(keys::DATA)
        .and_then(Value::as_array)
        .ok_or_else(|| CodecError::Malformed("image response requires a data array".into()))?
        .iter()
        .map(|d| {
            let b64 = d.get(super::B64_JSON).and_then(Value::as_str);
            let url = d.get(keys::URL).and_then(Value::as_str);
            if b64.is_none() && url.is_none() {
                return Err(CodecError::Malformed(
                    "image response entry requires b64_json or url".into(),
                ));
            }
            Ok(ImageOutput {
                b64: b64.map(str::to_string),
                url: url.map(str::to_string),
                revised_prompt: d
                    .get(super::REVISED_PROMPT)
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ..Default::default()
            })
        })
        .collect::<Result<_, CodecError>>()?;

    let mut warnings = Vec::new();
    let usage = read_image_usage(&v, &mut warnings);

    let cost_basis = if usage.is_none() {
        Some(crate::codec::ir::image::CostBasis {
            count: u32::try_from(images.len()).unwrap_or(u32::MAX),
            ..Default::default()
        })
    } else {
        None
    };
    Ok(ImageResp {
        created: v.get(super::CREATED).and_then(Value::as_u64),
        images,
        usage,
        cost_basis,
        warnings,
        ..Default::default()
    })
}

/// Wire -> concrete `ModerationReq` parse, extracted from the `OperationHandler::read_request`
pub fn read_moderation_request(
    body: &[u8],
    _content_type: &str,
) -> Result<crate::codec::ir::moderation::ModerationReq, IngressReject> {
    let wire: Value =
        serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
    let model = wire
        .get(keys::MODEL)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input = parse_input(wire.get(keys::INPUT))?;
    Ok(ModerationReq {
        model,
        input,
        extra: BTreeMap::new(),
    })
}

/// Wire -> concrete `ModerationResp` parse, extracted from the `OperationHandler::read_response`
pub fn read_moderation_response(
    wire: &[u8],
) -> Result<crate::codec::ir::moderation::ModerationResp, CodecError> {
    let v: Value =
        serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let results = v
        .get(keys::RESULTS)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CodecError::Malformed("moderation response requires a results array".into())
        })?
        .iter()
        .map(parse_result)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModerationResp {
        id: v.get(keys::ID).and_then(Value::as_str).map(str::to_string),
        model: v
            .get(keys::MODEL)
            .and_then(Value::as_str)
            .map(str::to_string),
        results,
        extra: BTreeMap::new(),
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
        moderation: Some(LeafCodec {
            write_request: write_moderation_request,
            write_response: write_moderation_response,
            read_request: read_moderation_request,
            read_response: read_moderation_response,
        }),
        ..crate::codec::leaf_codec::LeafCodecs::NONE
    };

#[cfg(test)]
#[path = "tests/audit_embedding_billing_tests.rs"]
mod audit_embedding_billing_tests;
