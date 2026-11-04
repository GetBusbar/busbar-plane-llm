//! The Gemini JSON-array stream framer (`:streamGenerateContent` without `?alt=sse`).

use super::*;
use crate::codec::keys;

/// Re-frame a Gemini SSE response stream as the JSON-array format a native `:streamGenerateContent`
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
    /// (empty for a partial frame, or frames with no data payload yet).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.aborted || self.finished {
            return Vec::new();
        }
        self.frames.push(chunk);
        let mut out: Vec<u8> = Vec::new();
        while let Some(frame) = self.frames.next_frame() {
            let Some((_event_type, data_str)) = parse_sse_frame(frame) else {
                continue;
            };
            if data_str.is_empty() || data_str == crate::codec::dialect::SSE_DONE_SENTINEL {
                continue;
            }
            let Ok(data) = crate::codec::json::parse_str::<serde_json::Value>(&data_str) else {
                self.aborted = true;
                break;
            };
            if self.started {
                out.push(b',');
            } else {
                out.push(b'[');
                self.started = true;
            }
            out.extend_from_slice(data.to_string().as_bytes());
        }

        if self.frames.pending() > Self::MAX_BUF {
            self.aborted = true;
            self.frames = SseFrames::new();
        }
        out
    }

    /// Call once at end-of-stream. Emits the closing `]` (and the opening `[` too, as `[]`, if no
    /// chunk was seen) so the body is always a parseable array.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        let buffer = self.frames.buffer();
        let pending = &buffer[buffer.len() - self.frames.pending()..];
        if self.aborted || pending.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return self.finish_with_error(500, GRPC_INTERNAL, "Internal error encountered.");
        }
        self.finished = true;
        if self.started {
            b"]".to_vec()
        } else {
            b"[]".to_vec()
        }
    }

    /// Close the array when this framer sits downstream of a cross-protocol [`StreamTranslate`]
    /// (gemini ingress, non-gemini egress).
    pub fn finish_for_translate(&mut self, translate_aborted: bool) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        if translate_aborted || self.aborted {
            return self.finish_with_error(500, GRPC_INTERNAL, "Internal error encountered.");
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

impl crate::codec::protocol::ArrayStreamFramer for GeminiJsonArrayFramer {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        GeminiJsonArrayFramer::feed(self, chunk)
    }

    fn finish_for_translate(&mut self, translate_aborted: bool) -> Vec<u8> {
        GeminiJsonArrayFramer::finish_for_translate(self, translate_aborted)
    }

    fn finish_with_server_error(&mut self, message: &str) -> Vec<u8> {
        GeminiJsonArrayFramer::finish_with_error(self, 500, GRPC_INTERNAL, message)
    }
}
