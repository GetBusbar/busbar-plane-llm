// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The (op, protocol) READ dispatch, test-only. Symmetric to the write dispatchers in
//! `leaf_codec`.
#![allow(dead_code)]

use super::*;

// Symmetric to the write dispatchers above. Production reads flow through the dialect vtable and the
// `Box<dyn IrHandle>` seam; these expose the SAME concrete parse the trait `read_*` delegates to
// (each dialect's `read_<op>_<dir>` free fn, as its `LeafCodecs` row declares it), so a leaf-op
// fidelity TEST can recover the concrete IR keyed by `(op, protocol)` without a downcast (the handle
// stays sealed). Not compiled in production.

/// The request read for `(op, proto)`; a protocol with no reader for the op is a `BadRequest`.
fn read_request<Q, A>(
    op: &str,
    proto: &str,
    pick: fn(&'static LeafCodecs) -> Option<&'static LeafCodec<Q, A>>,
    body: &[u8],
    content_type: &str,
) -> Result<Q, IngressReject> {
    match cell(proto, pick) {
        Some(c) => (c.read_request)(body, content_type),
        None => Err(IngressReject::BadRequest(format!(
            "no {op} reader for protocol `{proto}`"
        ))),
    }
}

/// The response read for `(op, proto)`; a protocol with no reader for the op is `Malformed`.
fn read_response<Q, A>(
    op: &str,
    proto: &str,
    pick: fn(&'static LeafCodecs) -> Option<&'static LeafCodec<Q, A>>,
    wire: &[u8],
) -> Result<A, CodecError> {
    match cell(proto, pick) {
        Some(c) => (c.read_response)(wire),
        None => Err(CodecError::Malformed(format!(
            "no {op} response reader for protocol `{proto}`"
        ))),
    }
}

pub fn embeddings_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<EmbeddingsReq, IngressReject> {
    read_request(
        "embeddings",
        proto,
        |l| l.embeddings.as_ref(),
        body,
        content_type,
    )
}
pub fn embeddings_read_response(proto: &str, wire: &[u8]) -> Result<EmbeddingsResp, CodecError> {
    read_response("embeddings", proto, |l| l.embeddings.as_ref(), wire)
}

pub fn rerank_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<RerankReq, IngressReject> {
    read_request("rerank", proto, |l| l.rerank.as_ref(), body, content_type)
}
pub fn rerank_read_response(proto: &str, wire: &[u8]) -> Result<RerankResp, CodecError> {
    read_response("rerank", proto, |l| l.rerank.as_ref(), wire)
}

pub fn image_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<ImageReq, IngressReject> {
    read_request("image", proto, |l| l.image.as_ref(), body, content_type)
}
pub fn image_read_response(proto: &str, wire: &[u8]) -> Result<ImageResp, CodecError> {
    read_response("image", proto, |l| l.image.as_ref(), wire)
}

pub fn transcription_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<TranscriptionReq, IngressReject> {
    read_request(
        "transcription",
        proto,
        |l| l.transcription.as_ref(),
        body,
        content_type,
    )
}
pub fn transcription_read_response(
    proto: &str,
    wire: &[u8],
) -> Result<TranscriptionResp, CodecError> {
    read_response("transcription", proto, |l| l.transcription.as_ref(), wire)
}

pub fn speech_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<SpeechReq, IngressReject> {
    read_request("speech", proto, |l| l.speech.as_ref(), body, content_type)
}
pub fn speech_read_response(proto: &str, wire: &[u8]) -> Result<SpeechResp, CodecError> {
    read_response("speech", proto, |l| l.speech.as_ref(), wire)
}

pub fn moderation_read_request(
    proto: &str,
    body: &[u8],
    content_type: &str,
) -> Result<ModerationReq, IngressReject> {
    read_request(
        "moderation",
        proto,
        |l| l.moderation.as_ref(),
        body,
        content_type,
    )
}
pub fn moderation_read_response(proto: &str, wire: &[u8]) -> Result<ModerationResp, CodecError> {
    read_response("moderation", proto, |l| l.moderation.as_ref(), wire)
}
