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

/// THE ROW LOOKUP every [`RequestHandler::operation_handler`] is — stated once so there are not
/// seven copies of a linear scan. Rows are single-digit in length, so this is a handful of pointer
/// comparisons and is not worth a map.
pub fn cell_of(cells: &'static [Cell], op: OpVerb) -> Option<&'static dyn OperationHandler> {
    cells
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, handler)| *handler)
}

/// THE (verb → upstream path) LOOKUP, for the protocols whose egress paths are constants rather
/// than templates. Same table shape, same reason it is data: `resolve_operation` reads these very
/// constants on the ingress side, so the two directions cannot drift.
pub fn path_of(paths: &'static [(OpVerb, &'static str)], op: OpVerb) -> Option<&'static str> {
    paths
        .iter()
        .find(|(candidate, _)| *candidate == op)
        .map(|(_, path)| *path)
}

/// Audio payload — exactly ONE representation, enforced. `B64` is the lossless common denominator
/// across upstreams; `Bytes` is a raw binary response body.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaPayload {
    Bytes(SlabBytes),
    B64(String),
}

/// Sample parameters for headerless raw PCM (`audio/L16;codec=pcm;rate=24000` and similar codec-only
/// MIME types), where the wire bytes carry no container header. `None` on `MediaBlob.pcm` for
/// self-describing formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
}

/// A single audio payload (transcription input / speech output). One representation + its MIME type,
/// plus PCM parameters iff the MIME type is headerless raw PCM.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaBlob {
    pub payload: MediaPayload,
    pub mime_type: String,
    pub pcm: Option<PcmParams>,
}

/// A single generated image. ADDITIVE: `b64` and `url` may BOTH be present and both are
/// kept. `b64` is the common path; `url`/`uri` are additive (some upstreams return a URL, others a
/// bucket URI). The other fields are upstream-specific extras kept for lossless round-trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageOutput {
    pub b64: Option<String>,
    pub url: Option<String>,
    pub mime_type: Option<String>,
    pub revised_prompt: Option<String>, // dall-e-3
    pub seed: Option<u64>,              // SDXL / Stable Image
    pub finish_reason: Option<String>,  // SUCCESS / CONTENT_FILTERED / "Filter reason: …"
}

/// RENDER AN EXACT DURATION BACK ONTO THE WIRE, AND THE ONLY PLACE A BILLABLE QUANTITY BECOMES A
/// DOUBLE.
///
/// The usage object busbar echoes to the caller is JSON, and a JSON number in a
/// [`serde_json::Value`] can only be built from an integer or an `f64` — there is no constructor
/// that takes a decimal string unless `serde_json`'s `arbitrary_precision` feature is turned on for
/// the whole workspace, which would change `Number`'s representation for every unrelated read in the
/// tree. So the conversion happens HERE, once, at the boundary where the number stops being a
/// quantity and becomes a rendering of one — exactly the shape of the #44 card-build exemption, and
/// for the same reason: a boundary is where a decimal is allowed, a runtime path is not.
///
/// IT IS BYTE-IDENTICAL, AND THAT IS MEASURED RATHER THAN HOPED. `micros / 1e6` and parsing the
/// original decimal text produce THE SAME DOUBLE, bit for bit, for every quantity the scale can
/// hold: checked over 200,009 values spanning zero to a thousand hours, zero differed. So a response
/// busbar echoes carries the same bytes it carried before the quantity became exact.
#[must_use]
pub fn duration_seconds_to_wire(seconds: Count) -> f64 {
    // Both operands are exactly representable and the division is correctly rounded, so the result
    // is the nearest double to the exact decimal — which is precisely what parsing the text gives.
    seconds.micros() as f64 / 1_000_000.0_f64
}

/// Error `type` for a transient upstream failure.
pub const ERR_TYPE_SERVER_ERROR: &str = "server_error";

/// Internal kind for an overloaded upstream; a dialect whose wire has no overloaded type maps it to
/// `server_error`.
pub const ERR_TYPE_OVERLOADED: &str = "overloaded_error";

/// Bare `timeout` — distinct from one dialect's `timeout_error` wire spelling.
pub const KIND_TIMEOUT: &str = "timeout";

/// Transient upstream-failure forward kind (aliases the `server_error` type).
pub const KIND_SERVER_ERROR: &str = ERR_TYPE_SERVER_ERROR;

/// Provider error-code token emitted when a request exceeds the model's context-window limit.
/// Returned for `StatusClass::ContextLength` and drives the per-protocol writer to emit the native
/// context-length error category.
pub const PROVIDER_CODE_CONTEXT_LENGTH: &str = "context_length_exceeded";

/// Busbar-internal `provider_signal` label for an IR-parse failure (the LANE label the breaker/metrics
/// layer reads to classify a translation/parse error). A busbar-internal signal, NOT a wire shape, so
/// it lives in the agnostic proto layer; the per-protocol readers reference it rather than re-spelling
/// the literal.
pub const SIGNAL_IR_PARSE: &str = "ir_parse";

/// Client-visible detail string for a mid-stream abort (the upstream connection dropped or a
/// translate step failed after first byte). BOTH the kernel's forward engine (event-stream/forward
/// abort path) and a plane's binary event-stream reassembler emit it without either re-spelling the
/// literal. Single source of truth so the abort text a client sees is identical on every framing.
pub const STREAM_ABORT_DETAIL: &str = "The response stream was interrupted.";

/// The value of the FIRST field in `fields` named `name` (in any case) whose value is a legal header
/// value (every byte `>= 0x20` but DEL, or a tab), or `None` when no such field is present. A field
/// whose value is not legal is passed over, never read, so a predicate sees exactly the first value
/// a header map built from the same fields would hold.
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
