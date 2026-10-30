// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The per-`(operation, egress-protocol)` leaf-op writer dispatch.
//!
//! Chat selects its writer by egress-protocol string (`proto_codec::protocol_for(proto).writer()`);
//! the six leaf ops (embeddings/image/rerank/transcription/speech/moderation) had each dialect's
//! write body inline in its `OperationHandler::{write_request,write_response}`, reachable only by
//! holding that handler. A leaf-op `IrHandle` cannot downcast (owner-forbidden) back to
//! `&EmbeddingsReq` to feed that writer.
//!
//! So each leaf op gets a writer selected by `(operation, egress-protocol)`: the per-dialect body is
//! a free fn in that dialect's `handler` module, and the dialect declares which ops it speaks as data,
//! its [`LeafCodecs`] row on its registration ([`crate::codec::proto_codec::DialectEntry::leaf`]). The
//! dispatchers find the egress dialect's row by name and call the fn it declared; none names a dialect
//! (design F3 self-contained). The dialect `OperationHandler`s route their own writes through these
//! (byte-identical), and the leaf-op `IrHandle::write_egress_request` calls the same dispatcher. Names
//! no `IrReq`/`IrResp`, moves no bytes.

use crate::codec::ir::audio::{SpeechReq, SpeechResp, TranscriptionReq, TranscriptionResp};
use crate::codec::ir::embeddings::{EmbeddingsReq, EmbeddingsResp};
use crate::codec::ir::image::{ImageReq, ImageResp};
use crate::codec::ir::moderation::{ModerationReq, ModerationResp};
use crate::codec::ir::rerank::{RerankReq, RerankResp};
use busbar_contract::codec::{CodecError, IngressReject, WireBody};
use bytes::Bytes;

/// One leaf operation's four codec fns in one dialect: the egress request writer, the ingress response
/// writer, and the two concrete reads (which the dialect's `OperationHandler` wraps in a leaf handle;
/// the `(op, protocol)` read dispatch below reaches them in test builds).
pub struct LeafCodec<Q: 'static, A: 'static> {
    pub write_request: fn(&Q) -> Bytes,
    pub write_response: fn(&A) -> WireBody,
    pub read_request: fn(&[u8], &str) -> Result<Q, IngressReject>,
    pub read_response: fn(&[u8]) -> Result<A, CodecError>,
}

/// A DIALECT'S ROW OF THE LEAF-OP SUPPORT MATRIX, as data: `Some` for each leaf op the dialect
/// speaks, `None` for the rest. Declared once in the dialect's own `handler` module.
pub struct LeafCodecs {
    pub embeddings: Option<LeafCodec<EmbeddingsReq, EmbeddingsResp>>,
    pub rerank: Option<LeafCodec<RerankReq, RerankResp>>,
    pub image: Option<LeafCodec<ImageReq, ImageResp>>,
    pub transcription: Option<LeafCodec<TranscriptionReq, TranscriptionResp>>,
    pub speech: Option<LeafCodec<SpeechReq, SpeechResp>>,
    pub moderation: Option<LeafCodec<ModerationReq, ModerationResp>>,
}

impl LeafCodecs {
    /// The row of a dialect that speaks no leaf op (chat only).
    pub const NONE: Self = Self {
        embeddings: None,
        rerank: None,
        image: None,
        transcription: None,
        speech: None,
        moderation: None,
    };
}

/// The `(operation, protocol)` cell: the named dialect's declared codec for the op `pick` selects.
/// `None` when no dialect has that name, or it declares no codec for the op.
fn cell<Q, A>(
    proto: &str,
    pick: fn(&'static LeafCodecs) -> Option<&'static LeafCodec<Q, A>>,
) -> Option<&'static LeafCodec<Q, A>> {
    crate::codec::proto_codec::entry_of(proto).and_then(|d| pick(d.leaf))
}

/// The write cell for `proto`. An unknown protocol, or one not speaking the op, is `unreachable!`:
/// every caller passes a real egress protocol for an op routed there, so a protocol added without
/// declaring the op fails loudly rather than emitting a malformed empty body.
fn write_cell<Q, A>(
    proto: &str,
    pick: fn(&'static LeafCodecs) -> Option<&'static LeafCodec<Q, A>>,
) -> &'static LeafCodec<Q, A> {
    cell(proto, pick).unwrap_or_else(|| unreachable!("leaf write: unknown egress protocol {proto}"))
}

/// Embeddings egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn embeddings_write_request(proto: &str, r: &EmbeddingsReq) -> Bytes {
    (write_cell(proto, |l| l.embeddings.as_ref()).write_request)(r)
}

/// Embeddings ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn embeddings_write_response(proto: &str, r: &EmbeddingsResp) -> WireBody {
    (write_cell(proto, |l| l.embeddings.as_ref()).write_response)(r)
}

/// Rerank egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn rerank_write_request(proto: &str, r: &RerankReq) -> Bytes {
    (write_cell(proto, |l| l.rerank.as_ref()).write_request)(r)
}

/// Rerank ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn rerank_write_response(proto: &str, r: &RerankResp) -> WireBody {
    (write_cell(proto, |l| l.rerank.as_ref()).write_response)(r)
}

/// Image egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn image_write_request(proto: &str, r: &ImageReq) -> Bytes {
    (write_cell(proto, |l| l.image.as_ref()).write_request)(r)
}

/// Image ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn image_write_response(proto: &str, r: &ImageResp) -> WireBody {
    (write_cell(proto, |l| l.image.as_ref()).write_response)(r)
}

/// Transcription egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn transcription_write_request(proto: &str, r: &TranscriptionReq) -> Bytes {
    (write_cell(proto, |l| l.transcription.as_ref()).write_request)(r)
}

/// Transcription ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn transcription_write_response(proto: &str, r: &TranscriptionResp) -> WireBody {
    (write_cell(proto, |l| l.transcription.as_ref()).write_response)(r)
}

/// Speech (TTS) egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn speech_write_request(proto: &str, r: &SpeechReq) -> Bytes {
    (write_cell(proto, |l| l.speech.as_ref()).write_request)(r)
}

/// Speech (TTS) ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn speech_write_response(proto: &str, r: &SpeechResp) -> WireBody {
    (write_cell(proto, |l| l.speech.as_ref()).write_response)(r)
}

/// Moderation egress request bytes for `proto` (see [`write_cell`] for an unknown protocol).
pub fn moderation_write_request(proto: &str, r: &ModerationReq) -> Bytes {
    (write_cell(proto, |l| l.moderation.as_ref()).write_request)(r)
}

/// Moderation ingress response wire for `proto` (see [`write_cell`] for an unknown protocol).
pub fn moderation_write_response(proto: &str, r: &ModerationResp) -> WireBody {
    (write_cell(proto, |l| l.moderation.as_ref()).write_response)(r)
}

// The (op, protocol) READ dispatch the leaf-op fidelity tests read through: test-only.
#[cfg(test)]
#[path = "tests/leaf_read.rs"]
mod leaf_read;
#[cfg(test)]
pub(crate) use leaf_read::*;
