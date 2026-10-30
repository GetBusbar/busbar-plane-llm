//! The Gemini JSON-array stream framer (`:streamGenerateContent` without `?alt=sse`).

use super::*;
use crate::codec::keys;

/// Re-frame a Gemini SSE response stream as the JSON-array format a native `:streamGenerateContent`
/// request without `?alt=sse` expects: a leading `[`, the per-chunk `GenerateContentResponse` objects
/// separated by `,`, a trailing `]`. Busbar always requests `?alt=sse` upstream, so the bytes reaching
/// this framer are Gemini SSE frames either way (same-protocol passthrough, or a cross-protocol
/// `StreamTranslate` whose ingress writer is gemini).
///
/// The JSON-array sibling of [`StreamTranslate`]'s SSE path: it strips the `data:` framing and
/// re-emits the payloads as one array. The output is always a valid JSON array (`finish` emits `]`,
/// or `[]` with no chunk), so a client that buffers and `JSON.parse`s the body succeeds.
pub struct GeminiJsonArrayFramer {
    /// The SSE frame reassembler the upstream bytes accumulate in.
    frames: SseFrames,
    /// Whether the opening `[` (and, for each later object, the separating `,`) has been emitted.
    started: bool,
    /// Set once `finish` has emitted the closing `]`, so a second `finish` is a no-op.
    finished: bool,
    /// Abandon the stream if the reassembly buffer grows past the cap with no complete frame.
    aborted: bool,
}

impl Default for GeminiJsonArrayFramer {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiJsonArrayFramer {
    // `pub(crate)` so the framer's tests can size a payload off the cap (the overflow abort path); not wire surface.
    pub const MAX_BUF: usize = crate::codec::eventstream::MAX_FRAME_BYTES;

    pub fn new() -> Self {
        Self {
            frames: SseFrames::new(),
            started: false,
            finished: false,
            aborted: false,
        }
    }

    /// Feed a chunk of Gemini SSE bytes; return JSON-array bytes for the complete frames now available
    /// (empty for a partial frame, or frames with no data payload yet). Each object is preceded by
    /// `[` (first) or `,` (subsequent).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.aborted || self.finished {
            return Vec::new();
        }
        self.frames.push(chunk);
        let mut out: Vec<u8> = Vec::new();
        while let Some(frame) = self.frames.next_frame() {
            let Some((_event_type, data_str)) = parse_sse_frame(frame) else {
                continue; // no data: line — keepalive/comment frame
            };
            if data_str.is_empty() || data_str == crate::codec::dialect::SSE_DONE_SENTINEL {
                continue; // egress terminator/keepalive — the array close is finish()'s job
            }
            let Ok(data) = crate::codec::json::parse_str::<serde_json::Value>(&data_str) else {
                continue;
            };
            if self.started {
                out.push(b',');
            } else {
                out.push(b'[');
                self.started = true;
            }
            out.extend_from_slice(data.to_string().as_bytes());
        }
        // Abandon the stream once the bytes no frame yielded pass the cap.
        if self.frames.pending() > Self::MAX_BUF {
            self.aborted = true;
            self.frames = SseFrames::new();
        }
        out
    }

    /// Call once at end-of-stream. Emits the closing `]` (and the opening `[` too, as `[]`, if no
    /// chunk was seen) so the body is always a parseable array. After an abort (the buffer overran
    /// `MAX_BUF` with no frame terminator) a bare `]` would make the truncated array look complete, so
    /// a Gemini-shaped `google.rpc.Status` error element is appended first.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        if self.aborted {
            return self.finish_with_error(
                500,
                GRPC_INTERNAL,
                // Client-facing body must carry no product/internal vocabulary (the
                // indistinguishability promise): "upstream" is busbar-internal and a fingerprintable
                // tell. Use Gemini's own canonical 500 `google.rpc.Status.message` instead.
                "Internal error encountered.",
            );
        }
        self.finished = true;
        if self.started {
            b"]".to_vec()
        } else {
            b"[]".to_vec()
        }
    }

    /// Close the array when this framer sits downstream of a cross-protocol [`StreamTranslate`]
    /// (gemini ingress, non-gemini egress). Like [`finish`] but it also surfaces a translate-side
    /// abort: when the translate's buffer overflowed `MAX_BUF` it stopped feeding this framer and its
    /// SSE error frame is not fed through (an SSE error cannot ride in a JSON-array body), so this
    /// framer's `aborted` stays clear and plain [`finish`] would emit a bare `]`, a silent truncation.
    /// Pass `translate_aborted = StreamTranslate::aborted()`; if either side aborted, the error element
    /// plus `]` is emitted (mirroring `StreamTranslate::finish`). Idempotent via `finished`.
    ///
    /// [`finish`]: Self::finish
    ///
    /// Production wiring is in `proxy engine`: on a normal close the `FirstByteBody` `Poll::Ready(None)`
    /// JSON-array arm feeds `translate.finish()`'s tail through [`Self::feed`] (the terminal usage
    /// frame becomes a trailing array element) then calls this with `translate.aborted()`; on an abort
    /// it skips the tail and relies on this error-close.
    pub fn finish_for_translate(&mut self, translate_aborted: bool) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        if translate_aborted || self.aborted {
            return self.finish_with_error(
                500,
                GRPC_INTERNAL,
                // Same client-facing body as the abort in `finish`: a native Gemini 500 message with no busbar vocabulary.
                "Internal error encountered.",
            );
        }
        self.finish()
    }

    /// Terminate the array with a trailing Gemini-shaped error element, then `]`. Used on a mid-stream
    /// transport failure (and internal abort): the in-band error must itself be a valid array element,
    /// a `{"error":{"code","message","status"}}` object matching `google.rpc.Status` (as
    /// `GeminiWriter::write_error` emits). Raw SSE `event:`/`data:` text here once spliced non-JSON
    /// into the array (an unparseable body and a protocol tell). Idempotent.
    pub fn finish_with_error(&mut self, code: u16, status: &str, message: &str) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let err = serde_json::json!({
            (keys::ERROR_WORD): { (keys::CODE): code, (keys::MESSAGE): message, (keys::STATUS): status }
        });
        let mut out: Vec<u8> = Vec::new();
        if self.started {
            out.push(b',');
        } else {
            out.push(b'[');
            self.started = true;
        }
        out.extend_from_slice(err.to_string().as_bytes());
        out.push(b']');
        out
    }
}

impl busbar_contract::protocol::ArrayStreamFramer for GeminiJsonArrayFramer {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        GeminiJsonArrayFramer::feed(self, chunk)
    }

    fn finish_for_translate(&mut self, translate_aborted: bool) -> Vec<u8> {
        GeminiJsonArrayFramer::finish_for_translate(self, translate_aborted)
    }

    fn finish_with_server_error(&mut self, message: &str) -> Vec<u8> {
        // The implementor owns the wire shape: a native Gemini server error is HTTP 500 / gRPC `INTERNAL`.
        // The agnostic caller passes only the message, so proxy engine names no Gemini value.
        GeminiJsonArrayFramer::finish_with_error(self, 500, GRPC_INTERNAL, message)
    }
}
