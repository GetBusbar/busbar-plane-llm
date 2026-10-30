// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The parts of the dialect framework only this plane names: the codec-row lookups, the media
//! carriers, the duration rendering, and the error kinds, signals and head reader its dialects
//! share. The shapes the kernel also reads (`ProtocolDecl`, `DialectCodec`, the handler traits,
//! the billing carriers, the upstream dispositions) stay in `busbar_contract`.

use busbar_contract::codec::{Cell, OperationHandler};
use busbar_contract::operation::OpVerb;
use busbar_contract::protocol::HeadFields;
use busbar_contract::{Count, SlabBytes};

/// The row lookup every [`RequestHandler::operation_handler`] is, stated once. Rows are
/// single-digit, so a few pointer comparisons beat a map.
pub fn cell_of(cells: &'static [Cell], op: OpVerb) -> Option<&'static dyn OperationHandler> {
    cells
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, handler)| *handler)
}

/// The (verb to upstream path) lookup for protocols whose egress paths are constants rather than
/// templates. `resolve_operation` reads the same constants inbound, so the directions cannot drift.
pub fn path_of(paths: &'static [(OpVerb, &'static str)], op: OpVerb) -> Option<&'static str> {
    paths
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, path)| *path)
}

/// Audio payload with exactly one representation, enforced. `B64` is the lossless common
/// denominator across upstreams; `Bytes` is a raw binary response body.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaPayload {
    Bytes(SlabBytes),
    B64(String),
}

/// Sample parameters for headerless raw PCM (`audio/L16;codec=pcm;rate=24000` and similar), where
/// the bytes carry no container header. `None` on `MediaBlob.pcm` for self-describing formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
}

/// A single audio payload (transcription input / speech output): one representation plus its MIME
/// type, plus PCM parameters iff the MIME type is headerless raw PCM.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaBlob {
    pub payload: MediaPayload,
    pub mime_type: String,
    pub pcm: Option<PcmParams>,
}

/// A single generated image. `b64` and `url` may both be present and are both kept (some upstreams
/// return a URL, others a bucket URI); the other fields are upstream extras kept for round-trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageOutput {
    pub b64: Option<String>,
    pub url: Option<String>,
    pub mime_type: Option<String>,
    pub revised_prompt: Option<String>, // dall-e-3
    pub seed: Option<u64>,              // SDXL / Stable Image
    pub finish_reason: Option<String>,  // SUCCESS / CONTENT_FILTERED / "Filter reason: …"
}

/// Render an exact duration back onto the wire: the only place a billable quantity becomes a double.
///
/// The echoed usage object is JSON, and a `serde_json::Value` number can only be built from an
/// integer or `f64` (no decimal-string constructor without `arbitrary_precision`, which would change
/// `Number` for every unrelated read workspace-wide). So the conversion happens here, once, where a
/// quantity becomes a rendering of one (the same reason as the #44 card-build exemption: a boundary
/// may hold a decimal, a runtime path may not).
///
/// It is byte-identical, measured: `micros / 1e6` and parsing the original decimal text give the same
/// double bit for bit for every quantity the scale holds (200,009 values from zero to a thousand
/// hours, zero differed), so an echoed response carries the bytes it did before the quantity was exact.
#[must_use]
pub fn duration_seconds_to_wire(seconds: Count) -> f64 {
    // Both operands are exactly representable and the division is correctly rounded, giving the
    // nearest double to the exact decimal, as parsing the text does.
    seconds.micros() as f64 / 1_000_000.0_f64
}

/// Error `type` for a transient upstream failure.
pub const ERR_TYPE_SERVER_ERROR: &str = "server_error";

/// Internal kind for an overloaded upstream; a dialect with no overloaded wire type maps it to `server_error`.
pub const ERR_TYPE_OVERLOADED: &str = "overloaded_error";

/// Bare `timeout`, distinct from one dialect's `timeout_error` wire spelling.
pub const KIND_TIMEOUT: &str = "timeout";

/// Transient upstream-failure forward kind (aliases the `server_error` type).
pub const KIND_SERVER_ERROR: &str = ERR_TYPE_SERVER_ERROR;

/// Provider error-code token for a request exceeding the model's context window. Returned for
/// `StatusClass::ContextLength`; the per-protocol writer emits the native context-length category.
pub const PROVIDER_CODE_CONTEXT_LENGTH: &str = "context_length_exceeded";

/// Busbar-internal `provider_signal` label for an IR-parse failure (the lane label the breaker and
/// metrics read). Not a wire shape; the readers reference it rather than re-spelling the literal.
pub const SIGNAL_IR_PARSE: &str = "ir_parse";

/// Client-visible detail for a mid-stream abort (upstream dropped or a translate step failed after
/// the first byte). The kernel's forward engine and a plane's binary event-stream reassembler both
/// emit it, so the text is identical on every framing.
pub const STREAM_ABORT_DETAIL: &str = "The response stream was interrupted.";

/// The value of the first field in `fields` named `name` (any case) whose value is a legal header
/// value (every byte `>= 0x20` but DEL, or a tab), else `None`. An illegal value is passed over, so a
/// predicate sees what a header map built from the same fields would hold.
#[must_use]
pub fn head_field<'a>(fields: HeadFields<'a>, name: &str) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|(n, v)| {
            n.eq_ignore_ascii_case(name.as_bytes())
                && v.iter().all(|b| (*b >= 0x20 && *b != 0x7f) || *b == b'\t')
        })
        .map(|(_, v)| *v)
}

#[cfg(test)]
#[path = "tests/framework_tests.rs"]
mod tests;
