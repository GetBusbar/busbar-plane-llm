// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Response-stream translation: the concrete `StreamTranslate` and its factory, driven by the
//! relay (`exchange::reply::relay`).

use crate::codec::dialect::{parse_sse_frame, write_sse_frame, SSE_DONE_FRAME, SSE_DONE_SENTINEL};
use crate::codec::framework::STREAM_ABORT_DETAIL;
use busbar_contract::protocol::{find_frame_terminator, IrError};
// Relative path so it resolves both standalone and netted into core.
use super::proto_codec::{protocol_for, Protocol, SameProtoEdit, StreamFraming, ToolIdRemap};
use std::borrow::Cow;

/// Pure cross-protocol response-stream translator. Feed egress-protocol SSE bytes, get ingress
/// SSE bytes: `egress.reader().read_response_events` (wire to IR, stateful fan-out) composed with
/// `ingress.writer().write_response_event` (IR to wire). Holds a reassembly buffer for frames
/// split across chunks and the IR decode state. Driven from the live streaming path (`FirstByteBody`).
pub struct StreamTranslate {
    ingress: Protocol,
    egress: Protocol,
    pub decode: crate::codec::ir::StreamDecodeState,
    pub buf: Vec<u8>,
    /// How far into `buf` we have scanned for a frame terminator; scanning only the tail keeps
    /// `feed()` linear when one large frame arrives in many small chunks.
    pub scanned: usize,
    /// Set once the reassembly buffer exceeds `MAX_BUF` with no complete frame: the stream is
    /// abandoned so an upstream that never emits a terminator cannot exhaust memory.
    pub aborted: bool,
    /// ingress == "openai": the stream must end with `data: [DONE]\n\n`.
    emit_done: bool,
    /// egress == "bedrock": frames are binary `application/vnd.amazon.eventstream`, not SSE.
    pub egress_eventstream: bool,
    /// ingress == "bedrock": the client is an AWS SDK, so each event is packed into a binary
    /// eventstream frame (valid CRC32), not SSE. The `messageStop`/`metadata` frames are the
    /// terminator (no `[DONE]`), so `finish()` stays empty. See `docs/architecture.md`.
    pub ingress_eventstream: bool,
    /// The host's monotonic reading (ns) when the first byte was fed, for a real `metrics.latencyMs`
    /// on a Bedrock-ingress `metadata` frame (a hard-coded `0` was a tell). Both ends use the
    /// host's clock (`CodecHost::monotonic_ns`); the plane reads no clock of its own. Set on the
    /// first `feed`; `None` before then or when the host has no clock.
    started_at: Option<u64>,
    /// Per-stream ingress-keyed framing state. The protocol-specific stream-shape decisions (OpenAI
    /// per-chunk identity replay and `include_usage` un-fold, Bedrock messageStop/metadata deferral
    /// and finish flush) live behind this vtable in the owning protocol's module
    /// ([`StreamFraming`]), so the translator names no wire quirk. Built from
    /// `ingress.writer().new_stream_framing()`; the default is the inert [`PassthroughFraming`].
    framing: Box<dyn StreamFraming>,
    /// Cross-protocol tool-id native remap: reshapes each egress `tool_use` id (OpenAI `call_…`) to
    /// the ingress client's native shape (Anthropic `toolu_…`) so a foreign id never reaches the
    /// client. Stream-scoped and deterministic, so the client's matching `tool_result` decodes back
    /// to the original egress id. The same-protocol path bypasses `translate_event`, so this runs
    /// only cross-protocol.
    tool_id_remap: ToolIdRemap,
    /// The events fed to `translate_ir_event` come from an answer that already went through the
    /// ingress preparation pass, so their tool ids are in the client's shape and must not be
    /// reshaped again.
    ///
    /// Only the buffered-answer synthesizer sets this. A second encode produced a doubly-wrapped id;
    /// the next turn's reverse decode strips one level, so the backend got an id it never issued and
    /// rejected the `tool_result`. The live wire-frame path feeds un-reshaped ids and leaves it `false`.
    ir_already_prepared: bool,
    /// Input-token usage captured at `MessageStart.usage`, carried to the terminal `MessageDelta`.
    ///
    /// Anthropic puts `usage.input_tokens` (and cache splits) only on `message_start`; its
    /// `message_delta` has `output_tokens` alone, whereas other protocols bundle both in the terminal
    /// usage. Out of an Anthropic backend the terminal `input_tokens` would be 0, under-reporting
    /// usage (and the `last_usage` billing reads). The start-usage input/cache fields are latched
    /// here and backfilled onto the terminal delta when it carries none. `None` until a
    /// usage-bearing `MessageStart`.
    start_usage: Option<crate::codec::ir::IrUsage>,
    /// Set once a terminal `MessageStop` was emitted. Guards against a trailing `MessageDelta`
    /// being written after the terminal frame on a non-eventstream ingress (e.g. an Anthropic
    /// `message_delta` after `message_stop`, invalid framing and a proxy tell). Usually a usage-only
    /// `MessageDelta{stop_reason: None}` (OpenAI `include_usage` sends totals after the finish
    /// chunk); a backend can also repeat a terminal delta. The Bedrock path folds late usage into
    /// its single `metadata` frame; elsewhere any post-stop `MessageDelta` is dropped.
    message_stopped: bool,
    /// Same-protocol universal-translate mode (Change B step 2): ingress == egress, built via
    /// [`StreamTranslate::new_same_proto`]. `feed` re-emits the original frame bytes verbatim
    /// (every frame is structurally pristine, so the short-circuit is unconditional). The IR
    /// pipeline still runs per frame as a side channel driving `last_usage` (the A-tap billing
    /// value); its serialized output is discarded.
    same_proto: bool,
    /// The terminal IR-derived usage (Change A "A-tap"), accumulated from the
    /// `MessageStart`/`MessageDelta`/`MessageStop` events after the Anthropic start-usage backfill,
    /// so it has the real prompt and completion counts for every protocol. The production billing
    /// source: `FirstByteBody`'s stream-end arm reads it via `usage()` for the per-request token fee.
    /// `None` until the first usage-bearing terminal event.
    last_usage: Option<crate::codec::ir::IrUsage>,
    /// A genuine terminal error seen mid-stream, set when a reader emits an
    /// [`crate::codec::ir::IrStreamEvent::Error`] (Anthropic `error`, OpenAI in-band `{"error":...}`,
    /// Responses `response.failed`, Bedrock `*Exception`). The breaker-failure signal the stream-end
    /// arm reads to tell a clean close from an aborted one. Holds the human message
    /// (`provider_signal`); `None` on a clean stream.
    terminal_error: Option<String>,
    /// Terminal-usage fold (SSE ingress: anthropic/gemini/cohere/responses; see
    /// `StreamFraming::folds_terminal_usage`). Holds the deferred terminal `MessageDelta`
    /// `(stop_reason, stop_sequence, usage, stop_detail)` so a trailing usage-only chunk (OpenAI
    /// `include_usage`) can merge into it before the `finish()` flush. `None` until the finish
    /// delta arrives (and for OpenAI/Bedrock ingress, which opt out). `pending_stop` records that
    /// the paired `MessageStop` was also deferred, so `finish()` re-emits it after the delta. The
    /// flush goes through the json-array framer too, reaching both SSE and gemini-json-array paths.
    pending_terminal: Option<(
        crate::codec::ir::IrStopReason,
        Option<String>,
        crate::codec::ir::IrUsage,
        Option<crate::codec::ir::IrStopDetail>,
    )>,
    pending_stop: bool,
    /// IR block indices whose BlockStart was translated and BlockStop has not. Anything left at the
    /// terminal frame was never closed by the egress reader (truncated upstream, or a terminal branch
    /// that does not drain every block, e.g. cohere `message-end` closes only text). Synthesized
    /// closes route through `emit_ir_event` so each writer applies its own projection.
    open_blocks: std::collections::BTreeSet<usize>,
    /// Test-only: counts frames that reached the `crate::codec::json::parse_str` DOM parse in the
    /// SSE loop, proving the same-proto read probe elides the parse for frames with no usage or
    /// failure. Compiled out of non-test builds.
    #[cfg(test)]
    pub decode_calls: std::cell::Cell<usize>,
}

impl StreamTranslate {
    /// Build a translator for an ingress to egress pair. `None` if either protocol is unknown or
    /// ingress == egress (the caller does native passthrough). Same-protocol callers wanting the
    /// verbatim path use [`new_same_proto`], so the `ingress == egress -> None` contract holds.
    pub fn new(ingress: &str, egress: &str) -> Option<Self> {
        if ingress == egress {
            return None;
        }
        Self::build(ingress, egress, false)
    }

    /// Build a same-protocol universal translator (Change B step 2). Unlike [`new`] it returns `Some`
    /// when `ingress == egress`. It runs the full reader-to-IR pipeline per frame for usage
    /// extraction but re-emits the original bytes verbatim (see `same_proto`). The caller gates it
    /// on `proxy::ENABLE_UNIVERSAL_SAME_PROTO_TRANSLATE`; off, it falls back to raw-chunk passthrough.
    pub fn new_same_proto(proto: &str) -> Option<Self> {
        Self::build(proto, proto, true)
    }

    /// Shared body of [`new`] and [`new_same_proto`]; `same_proto` selects the verbatim re-emit path in `feed`.
    fn build(ingress: &str, egress: &str, same_proto: bool) -> Option<Self> {
        let ingress_proto = protocol_for(ingress)?;
        let egress_proto = protocol_for(egress)?;
        // Framing flags come from the protocol vtable, not name comparisons: `ingress_eventstream`/
        // `egress_eventstream` reuse `ingress_is_eventstream()` (as `FirstByteBody` does, so they
        // cannot drift) and `emit_done` reads `emits_sse_done_terminator()`. No provider-name branch.
        let emit_done = ingress_proto
            .decl()
            .is_some_and(|d| d.emits_sse_done_terminator);
        let ingress_eventstream = ingress_proto
            .decl()
            .is_some_and(|d| d.ingress_is_eventstream);
        let egress_eventstream = egress_proto
            .decl()
            .is_some_and(|d| d.ingress_is_eventstream);
        // Framing is keyed to the ingress writer, which produces the client-facing wire.
        let framing = ingress_proto.writer().new_stream_framing();
        Some(Self {
            ingress: ingress_proto,
            egress: egress_proto,
            decode: crate::codec::ir::StreamDecodeState::default(),
            buf: Vec::new(),
            scanned: 0,
            aborted: false,
            emit_done,
            egress_eventstream,
            ingress_eventstream,
            started_at: None,
            framing,
            tool_id_remap: ToolIdRemap::default(),
            // The wire-frame paths read ids off the egress wire, so the remap runs; only the
            // buffered-answer synthesizer flips this.
            ir_already_prepared: false,
            start_usage: None,
            message_stopped: false,
            same_proto,
            last_usage: None,
            terminal_error: None,
            pending_terminal: None,
            pending_stop: false,
            open_blocks: std::collections::BTreeSet::new(),
            #[cfg(test)]
            decode_calls: std::cell::Cell::new(0),
        })
    }

    /// The translator whose ingress writer bounds each per-item accumulator at `cap`: the
    /// translation cap the plane enforces (`limits.request_body_max_bytes`). One never handed a cap
    /// holds [`busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT`].
    #[must_use]
    pub fn with_translate_cap(self, cap: usize) -> Self {
        self.ingress.writer().set_translate_cap(cap);
        self
    }

    /// Record whether the client opted into streaming usage (`stream_options.include_usage ==
    /// true`) and forward it to the ingress framing. Only OpenAI-ingress framing acts on it.
    /// Called by the engine before the first frame is fed.
    pub fn set_client_include_usage(&mut self, include: bool) {
        self.framing.set_client_include_usage(include);
    }

    /// Forward the original ingress request body to the ingress writer for the stream's life.
    /// Only `ResponsesWriter` latches it, so `response.created`/`response.completed` answer with
    /// the client's `temperature`/`top_p`/`instructions`/`metadata`/`tool_choice`/
    /// `parallel_tool_calls`/`tools` instead of spec defaults.
    pub fn set_request_echo(&mut self, ingress_request_body: &serde_json::Value) {
        self.ingress.writer().set_request_echo(ingress_request_body);
    }

    /// Translate one egress event `(event_type, payload)` into ingress wire bytes, advancing the
    /// decode state. Shared by the SSE and event-stream feed paths.
    fn translate_event(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        out: &mut Vec<u8>,
    ) {
        // `name_static(&self) -> &'static str` does not borrow `self`, so it does not conflict with
        // the mutable `self.tool_id_remap` borrow in `remap_event` (`Protocol::name` would).
        let ingress_name = self.ingress.name_static();
        // One translate attempt per stream: what the caller's dialect will not get is named once
        // per path (design F3 "Drops") and read back by [`Self::dropped`].
        let seam = crate::codec::drops::Seam {
            direction: crate::codec::drops::Direction::Response,
            ingress: ingress_name,
            egress: self.egress.name_static(),
        };
        let events = crate::codec::drops::read_stream_frame(
            host,
            seam,
            self.egress.reader(),
            event_type,
            data,
            &mut self.decode,
        );
        for ev in events {
            self.translate_ir_event(host, ingress_name, ev, out);
        }
    }

    /// Far-end wire paths this stream dropped on its way to the caller, each warned once; the host
    /// records one audit row per path at stream end.
    pub fn dropped(&self) -> &[String] {
        &self.decode.dropped
    }

    /// Run one decoded IR event through the cross-protocol pipeline (tool-id remap, identity strip,
    /// usage backfill and A-tap, framing seams, ingress writer) and append the framed bytes to
    /// `out`. Split from `translate_event` so the buffered-response synthesizer
    /// (`synthesize_from_response`) drives the same pipeline from an IR sequence.
    fn translate_ir_event(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        ingress_name: &'static str,
        mut ev: crate::codec::ir::IrStreamEvent,
        out: &mut Vec<u8>,
    ) {
        // Reshape the egress `tool_use` id on a `BlockStart` to the client's native shape
        // (`tool_id_remap`), before identity-strip and usage-backfill. Skipped for events from an
        // already-prepared answer (`ir_already_prepared`): reshaping is not idempotent (the reverse
        // decode peels one layer), so a second encode permanently corrupts tool_use/tool_result correlation.
        if !self.ir_already_prepared {
            self.tool_id_remap.remap_event(ingress_name, &mut ev);
            // IR-18: a signature the egress family minted that the client's carrier would read as its
            // own family's is wrapped in the busbar provenance envelope (stream half of
            // `chat_prepare_for_ingress`; an already-prepared answer carries it).
            if let crate::codec::ir::IrStreamEvent::BlockDelta {
                delta: crate::codec::ir::IrDelta::SignatureDelta(sig),
                ..
            } = &mut ev
            {
                let origin = self.egress.reader().stream_signature_origin(&self.decode);
                let writer = self.ingress.writer();
                *sig = crate::codec::ir::sig_envelope::for_client(sig, origin, |o| {
                    writer.reads_signature_origin_as_own(o)
                });
            }
        }
        // Identity strip: a `StreamTranslate` exists only when ingress != egress, so every event
        // crosses a boundary. Clear `MessageStart`'s foreign `id`/`created` so the ingress writer
        // mints native identity instead of leaking `chatcmpl-…`/`msg_…` (mirrors the non-stream
        // strip in proxy engine). `model` is kept on purpose: it is the lane's format-neutral name
        // and writers anchor the native stream-start skeleton on it (clearing it gave a degenerate
        // Anthropic `message_start` and a Gemini frame without `modelVersion`).
        if let crate::codec::ir::IrStreamEvent::MessageStart {
            id, created, usage, ..
        } = &mut ev
        {
            // Latch start-usage input/cache counts before stripping identity: Anthropic carries input
            // tokens only here, and the backfill below keeps them across the seam.
            if let Some(u) = usage {
                self.start_usage = Some(u.clone());
                // These counts are reported, so they bill even if the stream ends before its terminal
                // delta; fold them into `last_usage` now.
                self.fold_reported_usage(u);
            }
            *id = None;
            *created = None;
        }
        // Backfill the terminal usage: when the egress protocol reported input/cache tokens only at
        // start (Anthropic), restore them from the latched start-usage. Only empty fields are
        // filled, so protocols carrying input on the terminal delta (OpenAI include_usage, Gemini,
        // Bedrock, Cohere) are never overwritten.
        if let crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } = &mut ev {
            if let Some(start) = &self.start_usage {
                backfill_from_start_usage(usage, start);
            }
            // A-tap capture: accumulate the terminal IR usage after the backfill, merged per field
            // (keeping any non-zero / Some seen), so a backend splitting usage across two deltas
            // (OpenAI `include_usage`: zero-usage finish delta, then a usage-only one) does not let
            // the first zero clobber the second's counts. `last_usage` is the production billing source.
            self.fold_reported_usage(usage);
        }
        // A-tap terminal-error capture: a reader-emitted `Error` is the breaker-failure signal. Its
        // message is recorded so the stream-end arms treat the stream as failed (no token billing,
        // breaker transient), from the reader's structured decode, not a brace-scan of output bytes.
        if let crate::codec::ir::IrStreamEvent::Error(err) = &ev {
            self.terminal_error = Some(
                err.provider_signal
                    .clone()
                    .unwrap_or_else(|| "upstream stream error".to_string()),
            );
        }
        // Bedrock-ingress errors: an AWS SDK dispatches mid-stream errors off the
        // `:message-type: exception` / `:exception-type` headers that only `encode_exception_frame`
        // produces; a normal event frame would be silently dropped by a strict decoder. So for an
        // event-stream client an Error IR event goes through the writer's `write_response_exception`.
        if self.ingress_eventstream {
            if let crate::codec::ir::IrStreamEvent::Error(err) = &ev {
                if let Some((exc_name, message)) =
                    self.ingress.writer().write_response_exception(host, err)
                {
                    out.extend_from_slice(&crate::codec::eventstream::encode_exception_frame(
                        &exc_name, &message,
                    ));
                    return;
                }
            }
        }

        // INV-A: close any block the egress reader left open before the terminal frame. Placed ahead
        // of the Bedrock fan-out, terminal-usage deferral, post-stop guard and citation fan-out so
        // every terminal path reaches it once; a reader that closes correctly leaves `open_blocks`
        // empty, making it a no-op.
        if matches!(
            ev,
            crate::codec::ir::IrStreamEvent::MessageDelta {
                stop_reason: Some(_),
                ..
            } | crate::codec::ir::IrStreamEvent::MessageStop
        ) {
            self.close_open_blocks(host, out);
        }

        // Bedrock-ingress terminal fan-out. The IR carries one combined `MessageDelta{stop_reason,
        // usage}`, but an AWS SDK client expects a `messageStop` frame then a `metadata` frame (with
        // `metrics`). The single-return writer trait cannot emit two frames, so the combined delta is
        // split into a stop-only and a usage-only delta, with the real `metrics.latencyMs` injected on
        // the metadata frame; bedrock-to-bedrock still round-trips frame for frame. The framing
        // vtable decides what to emit and tracks the exactly-one-metadata invariant, so the translator
        // names no Bedrock wire shape (`PassthroughFraming` returns `None` for non-Bedrock ingress).
        if let crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: Some(reason),
            usage,
            stop_sequence,
            stop_detail,
        } = &ev
        {
            if let Some(mut events) =
                self.framing
                    .on_combined_stop_delta(*reason, stop_sequence.clone(), usage)
            {
                // The framing re-splits the terminal delta and names no `IrStopDetail`; the refinement
                // rides the stop-bearing frame it produced, so it is not lost (buffered == stream).
                for emit in &mut events {
                    if let crate::codec::ir::IrStreamEvent::MessageDelta {
                        stop_reason: Some(_),
                        stop_detail: d,
                        ..
                    } = emit
                    {
                        *d = stop_detail.clone();
                    }
                }
                for emit in &events {
                    self.emit_ir_event(host, emit, out);
                }
                return;
            }
        }
        // Terminal-usage fold (SSE ingress: anthropic/gemini/cohere/responses). These carry usage in
        // one terminal `message_delta`, but an OpenAI egress under include_usage reports it in a
        // separate trailing chunk after the finish chunk, which the post-stop guard below would drop.
        // So defer the terminal delta and its MessageStop, merge any trailing usage, and flush at
        // `finish()` (fed through the json-array framer too). OpenAI/Bedrock opt out
        // (`folds_terminal_usage == false`). Mutually exclusive with the Bedrock path above (which returned).
        if self.framing.folds_terminal_usage() {
            match &ev {
                crate::codec::ir::IrStreamEvent::MessageDelta {
                    stop_reason: Some(reason),
                    usage,
                    stop_sequence,
                    stop_detail,
                } => {
                    match self.pending_terminal.as_mut() {
                        // A duplicate stop-bearing delta (some providers repeat it): keep the first
                        // terminal's usage and merge non-zero fields from this one, rather than
                        // overwriting and dropping the client-visible usage. (found: 1.4.0, streaming-billing.)
                        Some((_, _, acc, _)) => merge_trailing_usage(acc, usage),
                        None => {
                            self.pending_terminal = Some((
                                *reason,
                                stop_sequence.clone(),
                                usage.clone(),
                                stop_detail.clone(),
                            ))
                        }
                    }
                    return;
                }
                crate::codec::ir::IrStreamEvent::MessageStop if self.pending_terminal.is_some() => {
                    self.pending_stop = true;
                    return;
                }
                crate::codec::ir::IrStreamEvent::MessageDelta {
                    stop_reason: None,
                    usage,
                    ..
                } if self.pending_terminal.is_some() => {
                    if let Some((_, _, acc, _)) = self.pending_terminal.as_mut() {
                        merge_trailing_usage(acc, usage);
                    }
                    return;
                }
                _ => {}
            }
        }
        if let crate::codec::ir::IrStreamEvent::MessageDelta {
            stop_reason: None, ..
        } = &ev
        {
            if let Some(should_emit) = self.framing.on_usage_only_delta() {
                if should_emit {
                    self.emit_ir_event(host, &ev, out);
                }
                return;
            }
        }

        // Non-eventstream ordering guard: once `MessageStop` is emitted, drop any trailing
        // `MessageDelta`, whether a usage-only one (OpenAI `include_usage` totals after the finish
        // chunk) or a duplicate terminal. Written, it would put `message_delta` after `message_stop`:
        // invalid framing and a proxy tell. The Bedrock path folded such usage into `metadata` above.
        if self.message_stopped
            && matches!(ev, crate::codec::ir::IrStreamEvent::MessageDelta { .. })
        {
            return;
        }
        if matches!(ev, crate::codec::ir::IrStreamEvent::MessageStop) {
            self.message_stopped = true;
        }

        // Multi-citation fan-out: one `BlockDelta{CitationsDelta(N)}` (e.g. a Gemini chunk batching
        // 3-10 `citationSources[]`) must not serialize to one wire event with a JSON array body where
        // the ingress frames one citation per event (an Anthropic SDK `JSON.parse`s one object per
        // `data:` line and crashes on an array). As in the Bedrock fan-out, split it into N
        // single-citation deltas here. The limit is a per-protocol wire constraint read via
        // `max_citations_per_delta()` (Anthropic: Some(1)); Gemini coalesces (None, no fan-out).
        if let Some(max_per_event) = self.ingress.decl().and_then(|d| d.max_citations_per_delta) {
            if let crate::codec::ir::IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
            } = &ev
            {
                if citations.len() > max_per_event {
                    for chunk in citations.chunks(max_per_event) {
                        let single = crate::codec::ir::IrStreamEvent::BlockDelta {
                            index: *index,
                            delta: crate::codec::ir::IrDelta::CitationsDelta(chunk.to_vec()),
                        };
                        self.emit_ir_event(host, &single, out);
                    }
                    return;
                }
            }
        }

        let is_message_start = matches!(ev, crate::codec::ir::IrStreamEvent::MessageStart { .. });
        self.emit_ir_event(host, &ev, out);
        // Some dialects emit a frame right after `message_start` (Anthropic's `event: ping`); omitting
        // it was a fingerprintable tell. The ingress writer decides which frame, if any; this only
        // places it. `translate_event` runs cross-protocol only, so same-protocol passthrough cannot
        // double-emit.
        if is_message_start {
            if let Some(frame) = self
                .ingress
                .decl()
                .and_then(|d| d.frame_after_message_start)
            {
                out.extend_from_slice(frame);
            }
        }
    }

    /// Re-emit a buffered (non-stream) 2xx as the ingress dialect's native stream, for a client that
    /// asked to stream when the upstream answered with one JSON body its stream decoder cannot parse.
    /// The translated response is fanned into the live event sequence (`response_to_ir_events`) and
    /// driven through the same per-event pipeline (`translate_ir_event`) and `finish()` (identity
    /// synthesis, framing quirks, terminal-usage fold, `[DONE]`), so the bytes match the live path.
    /// A native binary-eventstream ingress (Bedrock) keeps its own writer-side synthesizer and gets
    /// `None`, as does an unknown ingress. `translate_cap` is the plane's translation cap
    /// ([`Self::with_translate_cap`]).
    pub fn synthesize_from_response(
        host: &dyn busbar_contract::codec::CodecHost,
        ingress: &str,
        ir: &crate::codec::ir::IrResponse,
        translate_cap: usize,
    ) -> Option<Vec<u8>> {
        // Built with the ingress on both sides: the egress reader is not consulted (events come from
        // the IR) and `same_proto = false` keeps the writer live (verbatim re-emit needs original bytes).
        let mut st = Self::build(ingress, ingress, false)?.with_translate_cap(translate_cap);
        if st.ingress_eventstream {
            return None;
        }
        // The answer has been through the ingress preparation pass, which already reshaped its tool
        // ids to the client's form. Reshaping again strips only one layer on reverse decode, so the
        // client would echo an id the backend never issued. Tell the pipeline they are client-facing.
        st.ir_already_prepared = true;
        let ingress_name = st.ingress.name_static();
        let mut out: Vec<u8> = Vec::new();
        for ev in response_to_ir_events(ir) {
            st.translate_ir_event(host, ingress_name, ev, &mut out);
        }
        out.extend_from_slice(&st.finish(host));
        Some(out)
    }

    /// Fold a far-end-reported usage into `last_usage`, the billing source: the one per-field
    /// non-zero/`Some`-wins merge (shared with the terminal-usage fold), so the cross- and
    /// same-protocol A-taps cannot drift and the detail buckets ride along. Fed by every reported
    /// usage, stream-start counts (Anthropic `message_start`) included, so a stream that ends before
    /// its terminal frame bills what was reported (#62).
    fn fold_reported_usage(&mut self, reported: &crate::codec::ir::IrUsage) {
        let acc = self
            .last_usage
            .get_or_insert(crate::codec::ir::IrUsage::default());
        merge_trailing_usage(acc, reported);
    }

    /// Same-protocol usage side-channel (Change B step 2): runs only the egress reader and the
    /// start-usage/backfill/`last_usage` accumulation of `translate_event`, with no remap, identity
    /// strip, writer or reframe, since the original bytes re-emit verbatim. That keeps the
    /// short-circuit at or below the cost of not translating (the benchmark gate). The reader owns
    /// the decode state, which still advances for multi-frame streams.
    fn extract_usage_only(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
    ) {
        for ev in
            self.egress
                .reader()
                .read_response_events(host, event_type, data, &mut self.decode)
        {
            if let crate::codec::ir::IrStreamEvent::MessageStart { usage: Some(u), .. } = &ev {
                self.start_usage = Some(u.clone());
                self.fold_reported_usage(u);
            }
            // A-tap terminal-error capture on the same-protocol path: the original error frame
            // re-emits verbatim, but the breaker/billing arms still need to know the stream aborted.
            if let crate::codec::ir::IrStreamEvent::Error(err) = &ev {
                self.terminal_error = Some(
                    err.provider_signal
                        .clone()
                        .unwrap_or_else(|| "upstream stream error".to_string()),
                );
            }
            if let crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } = &ev {
                // The exact accumulation `translate_event` runs: start-usage backfill, then the one
                // shared Some-wins fold. A hand-rolled four-field copy here once discarded every
                // `IrUsageDetail` bucket, so a same-protocol stream reported no attribution (and a
                // streamed Cohere call metered RAW totals where buffered meters `billed_units`).
                let mut trailing = usage.clone();
                if let Some(start) = &self.start_usage {
                    backfill_from_start_usage(&mut trailing, start);
                }
                self.fold_reported_usage(&trailing);
            }
        }
    }

    /// Write one IR event through the ingress writer and append its framed bytes to `out`. Branches
    /// only on transport (binary frame vs SSE); every protocol-specific per-frame decision (a native
    /// usage frame's real latency, per-chunk SSE identity) goes through the [`StreamFraming`] vtable.
    fn emit_ir_event(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &crate::codec::ir::IrStreamEvent,
        out: &mut Vec<u8>,
    ) {
        // Track the IR lifecycle before the writer runs: `write_response_event` returning `None` does
        // not mean no block opened. `gemini/writer.rs`'s ToolUse `BlockStart` returns `None` on
        // purpose (it buffers name/args and emits the `functionCall` part on `BlockStop`); tracking
        // afterwards would never record it, the terminal drain would not close it, and on a truncated
        // cohere-to-gemini stream the tool call would be silently deleted. Track the IR intent.
        match ev {
            crate::codec::ir::IrStreamEvent::BlockStart { index, .. } => {
                self.open_blocks.insert(*index);
            }
            crate::codec::ir::IrStreamEvent::BlockStop { index } => {
                self.open_blocks.remove(index);
            }
            _ => {}
        }
        // One IR event may frame as several wire events: the Responses writer expands a text
        // `BlockStart`/`BlockStop` into the content-part brackets a strict SDK requires
        // (`ProtocolWriter::write_response_events`). Other writers yield 0 or 1. Each frame goes
        // through the transport/vtable split below independently.
        for (out_et, mut out_data) in self.ingress.writer().write_response_events(host, ev) {
            if self.ingress_eventstream {
                // Bedrock-ingress client: pack the event into a binary eventstream frame (valid
                // CRC32). Per-frame metrics (a usage frame's real latency) come through the framing
                // vtable. u128 to u64 for JSON, saturating.
                let elapsed_ms = self
                    .started_at
                    .zip(host.monotonic_ns())
                    .map(|(start, now)| now.saturating_sub(start) / 1_000_000);
                self.framing
                    .inject_streaming_metrics(&out_et, &mut out_data, elapsed_ms);
                let payload = crate::codec::json::to_vec(&out_data).unwrap_or_default();
                // Usage in this frame was already accumulated into `last_usage` from the IR event
                // before the writer ran, so billing needs no pre-encode side channel. Just encode.
                out.extend_from_slice(&crate::codec::eventstream::encode_frame(&out_et, &payload));
            } else {
                // Egress-chunk framing seam: the OpenAI per-chunk identity replay and `include_usage`
                // trailing-usage un-fold live behind the framing vtable. It mutates the chunk in place
                // and, for a usage-bearing finish chunk, returns a trailing usage-only chunk to frame
                // after it (two-frame order preserved). `PassthroughFraming` is inert. The `[DONE]`
                // terminator stays a separate `finish()` literal.
                if let Some(trailing) = self.framing.on_egress_chunk(&mut out_data) {
                    write_sse_frame(out, &out_et, &out_data);
                    write_sse_frame(out, &out_et, &trailing);
                    continue;
                }
                write_sse_frame(out, &out_et, &out_data);
            }
        }
    }

    /// Close every block the egress reader left open. `mem::take` first so re-entrant
    /// `emit_ir_event` calls cannot re-insert (idempotent). `BTreeSet` order gives ascending indices.
    fn close_open_blocks(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        out: &mut Vec<u8>,
    ) {
        for index in std::mem::take(&mut self.open_blocks) {
            self.emit_ir_event(
                host,
                &crate::codec::ir::IrStreamEvent::BlockStop { index },
                out,
            );
        }
    }

    /// Hard cap on the reassembly buffer, so an upstream that never emits a frame terminator cannot
    /// exhaust memory. Defined as `eventstream::MAX_FRAME_BYTES` (single source of truth): a smaller
    /// cap would abort a frame the binary decoder accepts before `drain_frames` saw it, and
    /// diverging literals would reintroduce that with no compile-time signal.
    pub const MAX_BUF: usize = crate::codec::eventstream::MAX_FRAME_BYTES;

    /// Feed a chunk of egress SSE bytes; return translated ingress bytes for the complete frames now
    /// available (empty for a partial frame). Past [`Self::MAX_BUF`] with no complete frame the
    /// stream is abandoned and further input ignored.
    pub fn feed(&mut self, host: &dyn busbar_contract::codec::CodecHost, chunk: &[u8]) -> Vec<u8> {
        self.feed_borrowed(host, chunk).into_owned()
    }

    /// [`Self::feed`], borrowing where it can: a same-protocol SSE piece on a frame boundary
    /// (nothing buffered) needing no edit is answered as `chunk[..consumed]` with no copy; only an
    /// unterminated tail is copied into the reassembly buffer.
    pub fn feed_borrowed<'a>(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        chunk: &'a [u8],
    ) -> Cow<'a, [u8]> {
        if self.aborted {
            return Cow::Borrowed(&[]);
        }
        // Stamp the stream start on the first byte, on the host's monotonic clock, so a Bedrock-ingress
        // `metadata` frame reports a real `metrics.latencyMs`. One host read per stream.
        if self.started_at.is_none() && self.ingress_eventstream {
            self.started_at = host.monotonic_ns();
        }
        if self.egress_eventstream {
            return Cow::Owned(self.feed_eventstream(host, chunk));
        }
        // The buffer is taken out for the drain so its borrowed frames can be read while reader state advances.
        let mut buf = std::mem::take(&mut self.buf);
        let fresh = buf.is_empty();
        let (consumed, edited) = if fresh {
            self.drain_sse(host, chunk)
        } else {
            buf.extend_from_slice(chunk);
            self.drain_sse(host, &buf)
        };
        let out = match edited {
            Some(out) => Cow::Owned(out),
            None if fresh => Cow::Borrowed(&chunk[..consumed]),
            None => Cow::Owned(buf[..consumed].to_vec()),
        };
        // Keep only the unterminated tail: the consumed prefix is reclaimed in one linear shift.
        if fresh {
            buf.extend_from_slice(&chunk[consumed..]);
        } else {
            buf.drain(..consumed);
        }
        self.scanned = buf.len();
        self.buf = buf;
        if self.buf.len() > Self::MAX_BUF {
            self.abort();
        }
        out
    }

    /// Egress is binary AWS event-stream framing (Bedrock ConverseStream). The event name is in the
    /// `:event-type` header, not the payload; the Bedrock reader keys off a `type` field, so the
    /// header is folded into the payload.
    fn feed_eventstream(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        chunk: &[u8],
    ) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out: Vec<u8> = Vec::new();
        // `drain_frames_checked` (not the discarding `drain_frames`) so a malformed egress prelude
        // (out-of-range `total_len` / oversized `headers_len`) surfaces as an explicit
        // `DrainStatus::MalformedPrelude` abort, not a silent clear-and-stop.
        //
        // Same-protocol bedrock-to-bedrock re-emits the original frame bytes verbatim, never
        // re-encoded: `encode_frame` recomputes length and CRC32 and any divergence (key order,
        // float formatting) makes an undecodable frame for an AWS SDK. The drain writes each complete
        // valid frame's bytes straight into `out` via `consumed_sink` (no per-chunk clone), and never
        // a cleared malformed-prelude remainder. Cross-protocol the sink is `None`.
        let (frames, status, _valid_consumed) = crate::codec::eventstream::drain_frames_checked(
            &mut self.buf,
            if self.same_proto {
                Some(&mut out)
            } else {
                None
            },
        );
        for (event_type, payload) in frames {
            // Same-proto: the frame's bytes are already out; the payload is read only if its event
            // type can carry usage or failure.
            if self.same_proto && !self.egress.reader().same_proto_reads(&event_type, &payload) {
                continue;
            }
            let Ok(mut data) = serde_json::from_slice::<serde_json::Value>(&payload) else {
                continue; // non-JSON payload — skip the frame
            };
            if let Some(obj) = data.as_object_mut() {
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String(event_type.clone()),
                );
            }
            if self.same_proto {
                // Same-proto: run only the reader and usage accumulation (`extract_usage_only`).
                self.extract_usage_only(host, &event_type, &data);
            } else {
                self.translate_event(host, &event_type, &data, &mut out);
            }
        }
        // A malformed prelude is unrecoverable: abandon the stream like the MAX_BUF overflow so
        // `finish()` emits the terminal exception frame (driven by `aborted`), not a silent truncation.
        if status == crate::codec::eventstream::DrainStatus::MalformedPrelude
            || self.buf.len() > Self::MAX_BUF
        {
            self.abort();
        }
        out
    }

    /// Drain every complete blank-line-delimited SSE frame of `src` (the buffer, or a piece on a
    /// frame boundary). Cross-protocol each frame is translated; same-protocol each is read only for
    /// the usage side channel ([`super::proto_codec::ProtocolReader::same_proto_reads`] skips the
    /// rest unparsed) and re-emitted verbatim, byte for byte (comments, keepalives, `[DONE]`, line
    /// shape, terminator), except a frame the ingress framing edits
    /// ([`StreamFraming::same_proto_edit`]). Returns how many bytes of `src` were complete frames,
    /// and the bytes the caller reads: `None` when those are exactly `src[..consumed]`.
    ///
    /// LF-LF, CRLF and bare-CR terminators are all recognized (CRLF SSE from gateways/CDNs would
    /// otherwise buffer to MAX_BUF and abort). `consumed` is a front cursor the caller reclaims once.
    /// The search floor is `consumed`, never below it (that re-finds the consumed terminator: an
    /// empty frame and an infinite loop); the 3-byte backup (a CRLF split across chunks) and the
    /// `scanned` skip apply only above it.
    fn drain_sse(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        src: &[u8],
    ) -> (usize, Option<Vec<u8>>) {
        let mut consumed = 0usize;
        let mut out: Vec<u8> = Vec::new();
        // Same-protocol: start of the not-yet-flushed verbatim run, split only by an edited frame.
        let mut emit_from = 0usize;
        let mut edited = !self.same_proto;
        loop {
            let search_from = self.scanned.saturating_sub(3).max(consumed).min(src.len());
            let Some((rel, term_len)) = find_frame_terminator(&src[search_from..]) else {
                break;
            };
            let end = search_from + rel + term_len;
            let frame_start = consumed;
            let frame = &src[frame_start..end];
            consumed = end;
            self.scanned = end;
            let Some((event_type, data)) = parse_sse_frame(frame) else {
                continue; // no data: line, or non-utf8 — skip
            };
            if data.is_empty() || data == SSE_DONE_SENTINEL {
                // Egress terminator/keepalive (ingress's is `finish()`'s). Carries no usage on any
                // current protocol; still re-emitted verbatim same-protocol.
                continue;
            }
            if !self.same_proto {
                #[cfg(test)]
                self.decode_calls.set(self.decode_calls.get() + 1);
                let Ok(value) = crate::codec::json::parse_str::<serde_json::Value>(&data) else {
                    continue; // malformed data JSON — skip the frame rather than abort
                };
                self.translate_event(host, event_type, &value, &mut out);
                continue;
            }
            let reads = self
                .egress
                .reader()
                .same_proto_reads(event_type, data.as_bytes());
            if reads {
                #[cfg(test)]
                self.decode_calls.set(self.decode_calls.get() + 1);
                let Ok(value) = crate::codec::json::parse_str::<serde_json::Value>(&data) else {
                    continue; // malformed data JSON — relayed as it came, unread
                };
                self.extract_usage_only(host, event_type, &value);
            }
            // The framing's edit (for an opted-out OpenAI client: drop the unsolicited trailing usage
            // chunk, strip the forced `"usage":null`) applies only to a payload that parses; others
            // relay as they came. Flush the verbatim run, emit the edit, resume after it.
            let edit = self.framing.same_proto_edit(data.as_bytes());
            if edit == SameProtoEdit::Keep
                || (!reads
                    && crate::codec::json::parse_str::<serde::de::IgnoredAny>(&data).is_err())
            {
                continue;
            }
            edited = true;
            out.extend_from_slice(&src[emit_from..frame_start]);
            if edit == SameProtoEdit::StripUsage {
                rewrite_frame_strip_usage(&mut out, frame, &data);
            }
            emit_from = end;
        }
        if self.same_proto && edited {
            out.extend_from_slice(&src[emit_from..consumed]);
        }
        (consumed, edited.then_some(out))
    }

    /// True once this translator abandoned its stream (reassembly buffer past [`Self::MAX_BUF`] with
    /// no terminator, or a malformed egress event-stream prelude), so the happy-path terminal events
    /// were never produced and further `feed`s are no-ops.
    ///
    /// SSE ingress: `finish()` already surfaces the abort as the native in-band error frame. Gemini
    /// JSON-array ingress: the close arm normally feeds `finish()`'s tail through
    /// [`GeminiJsonArrayFramer::feed`] then calls [`GeminiJsonArrayFramer::finish_for_translate`], but
    /// on abort the SSE bytes are an error frame that cannot ride inside a JSON array, and the
    /// framer's own `aborted` flag is unset (translate just stopped feeding), so a bare `]` would
    /// swallow the truncation. This accessor lets the close path emit the framer error-close.
    /// Wired in `proxy engine`: the `FirstByteBody` `Poll::Ready(None)` JSON-array close arm passes
    /// `translate.aborted()` to `framer.finish_for_translate(translate_aborted)`.
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// The terminal IR-derived usage (Change A "A-tap"), or `None` if no usage-bearing terminal event
    /// was seen. The production billing source: the `FirstByteBody` stream-end arm reads it for the
    /// per-request token fee. Populated by `translate_event` / `extract_usage_only` after the
    /// Anthropic start-usage backfill. See the `last_usage` field.
    pub fn usage(&self) -> Option<&crate::codec::ir::IrUsage> {
        self.last_usage.as_ref()
    }

    /// The terminal stream error message, or `None` for a clean stream (Change A). The
    /// `FirstByteBody` stream-end arm reads it for breaker disposition (non-`None` records a breaker
    /// transient and suppresses token billing). Set when a reader emits an
    /// [`crate::codec::ir::IrStreamEvent::Error`]. See `terminal_error`.
    pub fn terminal_error(&self) -> Option<&str> {
        self.terminal_error.as_deref()
    }

    /// Frame a terminal mid-stream error through the live ingress writer this translator has driven
    /// since the first frame, the one that latched the stream's identity. A fresh writer from the
    /// registry restarts every per-stream cell: on a Responses ingress the failure would carry
    /// `sequence_number: 0` and an unrelated response id, which a strict decoder cannot reconcile.
    /// The live writer replays the latched identity and advances the same counter, as the in-band
    /// abort frame from `finish()` does. `None` when the writer frames no in-band error; the caller
    /// falls back to the dialect seam.
    pub fn terminal_error_frame(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        self.ingress.writer().write_error_frame(host, err)
    }

    /// Abandon the stream as unrecoverable: release the reassembly buffer, set `aborted` so further
    /// `feed()` is a no-op, and let `finish()` emit the ingress-native terminal error frame. Triggers
    /// are a buffer past [`Self::MAX_BUF`] without a terminator and a malformed egress event-stream
    /// prelude ([`crate::codec::eventstream::DrainStatus::MalformedPrelude`]); both must surface an
    /// error, never a silent truncation.
    fn abort(&mut self) {
        self.aborted = true;
        self.buf.clear();
        self.buf.shrink_to_fit();
        self.scanned = 0;
    }

    /// Call once at end-of-stream. Returns the ingress terminator (OpenAI: `data: [DONE]\n\n`;
    /// Anthropic: empty, `message_stop` already terminates).
    pub fn finish(&mut self, host: &dyn busbar_contract::codec::CodecHost) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        // Bedrock-ingress abort: the buffer overflowed `MAX_BUF` with no terminator, so no
        // `messageStop`/`metadata` was translated. A real ConverseStream always ends with
        // messageStop+metadata or an exception frame, so a bare close is a tell and leaves an SDK in
        // an ambiguous state. Emit a modeled `InternalServerException` frame, mirroring the
        // transport-error path in proxy engine (`mid_stream_error_bytes`). The only terminator on an
        // aborted stream, so return early.
        if self.aborted {
            // The shared abort detail (`STREAM_ABORT_DETAIL`) keeps the client-visible text identical
            // to proxy engine's SSE/forward abort path.
            const ABORT_DETAIL: &str = STREAM_ABORT_DETAIL;
            if let Some(exc_type) = self.framing.abort_exception_type() {
                // The ingress framing owns the wire exception type name (Bedrock:
                // `InternalServerException`); `Some` here is the eventstream-ingress abort signal.
                out.extend_from_slice(&crate::codec::eventstream::encode_exception_frame(
                    exc_type,
                    ABORT_DETAIL,
                ));
                return out;
            }
            // SSE-ingress abort: the buffer overflowed `MAX_BUF`, so the happy-path terminal events
            // were never produced. A bare close would look like a successful short completion, so emit
            // the ingress protocol's native in-band error frame (the same
            // `write_response_event(&IrStreamEvent::Error(..))` path as `proxy::mid_stream_error_bytes`'s
            // SSE branch), framed as SSE text by `emit_ir_event`.
            let err = IrError {
                class: busbar_contract::upstream::StatusClass::ServerError,
                provider_signal: Some(ABORT_DETAIL.to_string()),
                retry_after: None,
            };
            let ev = crate::codec::ir::IrStreamEvent::Error(err);
            self.emit_ir_event(host, &ev, &mut out);
            // OpenAI ingress still ends with `data: [DONE]\n\n` after the error frame, as a genuine
            // stream does (the error is an in-band `data:` chunk, not the terminator).
            if self.emit_done {
                out.extend_from_slice(SSE_DONE_FRAME);
            }
            return out;
        }
        // Same-protocol verbatim mode: the upstream's own terminator (`[DONE]`, the final bedrock
        // frames) already rode through `feed`, so `finish` adds nothing on the happy path (a synthetic
        // one would duplicate it). The abort branch above still fires so a truncated stream surfaces
        // an in-band error.
        if self.same_proto {
            return out;
        }
        // INV-A: close any block the egress reader left open, before `framing.on_finish()`: on
        // truncation no terminal frame arrived, and some readers' terminal branches do not drain every
        // block (cohere `message-end` closes only text; `open_tools` is never drained). Unconditional;
        // the `pending_terminal` flush below is dead for these because `pending_terminal` is only set
        // when `folds_terminal_usage()` is true, which bedrock and openai_chat do not.
        self.close_open_blocks(host, &mut out);
        // Finish framing seam: a `metadata` frame the ingress framing deferred and never resolved
        // (Bedrock-ingress zero-usage stop with no trailing usage delta, the default OpenAI streaming
        // case) is flushed here as one best-effort zero-usage event. A real ConverseStream always ends
        // with `metadata`, so this is closer to native than omitting it. `PassthroughFraming` returns `None`.
        if let Some(trailing) = self.framing.on_finish() {
            self.emit_ir_event(host, &trailing, &mut out);
        }
        // Terminal-usage fold flush (SSE ingress): the terminal `message_delta` deferred in `feed`
        // (see `folds_terminal_usage`) is emitted now with the merged usage, then its `MessageStop`.
        // Without a trailing usage chunk it is just the terminal frame deferred to end of stream
        // (nothing follows a stop). The body feeds this output through the json-array framer too.
        if let Some((reason, stop_sequence, usage, stop_detail)) = self.pending_terminal.take() {
            self.emit_ir_event(
                host,
                &crate::codec::ir::IrStreamEvent::MessageDelta {
                    stop_reason: Some(reason),
                    stop_sequence,
                    usage,
                    stop_detail,
                },
                &mut out,
            );
            if self.pending_stop {
                self.emit_ir_event(
                    host,
                    &crate::codec::ir::IrStreamEvent::MessageStop,
                    &mut out,
                );
            }
        }
        if self.emit_done {
            out.extend_from_slice(SSE_DONE_FRAME);
        }
        out
    }
}

/// Fan a whole (buffered) response out into the IR events a live stream of the same completion
/// carries: `MessageStart`, per block a `BlockStart`, delta(s) and `BlockStop`, then the terminal
/// `MessageDelta` and `MessageStop`, so the ingress writer sees the live-path shape. Indices advance
/// only for blocks that emit events (tool result / image / media / raw JSON have no streamed analog
/// and are skipped), so the stream has no gaps a native one would not.
pub fn response_to_ir_events(
    ir: &crate::codec::ir::IrResponse,
) -> Vec<crate::codec::ir::IrStreamEvent> {
    use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStreamEvent};
    let mut events: Vec<IrStreamEvent> = Vec::new();
    events.push(IrStreamEvent::MessageStart {
        role: ir.role,
        // Usage rides the terminal delta.
        usage: None,
        id: ir.id.clone(),
        created: ir.created,
        model: ir.model.clone(),
    });
    let mut index = 0usize;
    let mut logprobs_pending = !ir.logprobs.is_empty();
    for block in ir.content.iter() {
        match block {
            IrBlock::Text {
                text,
                citations,
                refusal,
                ..
            } => {
                // A refusal message stays a refusal (IR-02).
                events.push(IrStreamEvent::BlockStart {
                    index,
                    block: IrBlockMeta::Text,
                    refusal: *refusal,
                });
                events.push(IrStreamEvent::BlockDelta {
                    index,
                    delta: IrDelta::TextDelta(text.clone()),
                });
                if !citations.is_empty() {
                    events.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::CitationsDelta(citations.clone()),
                    });
                }
                // Per-token logprobs belong to the generated text: attached to the first text block.
                if logprobs_pending {
                    logprobs_pending = false;
                    events.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::LogprobsDelta(ir.logprobs.clone()),
                    });
                }
                events.push(IrStreamEvent::BlockStop { index });
                index += 1;
            }
            IrBlock::ToolUse {
                id, name, input, ..
            } => {
                events.push(IrStreamEvent::BlockStart {
                    index,
                    block: IrBlockMeta::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                    },
                    refusal: false,
                });
                events.push(IrStreamEvent::BlockDelta {
                    index,
                    delta: IrDelta::InputJsonDelta(input.to_string()),
                });
                events.push(IrStreamEvent::BlockStop { index });
                index += 1;
            }
            IrBlock::Thinking {
                text,
                signature,
                redacted,
                kind,
                ..
            } => {
                events.push(IrStreamEvent::BlockStart {
                    index,
                    block: if *redacted {
                        IrBlockMeta::RedactedThinking
                    } else {
                        // IR-17: the block's kind rides the stream start.
                        IrBlockMeta::Thinking { kind: *kind }
                    },
                    refusal: false,
                });
                if *redacted {
                    events.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::RedactedReasoningDelta(text.clone()),
                    });
                } else {
                    events.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: IrDelta::ThinkingDelta(text.clone()),
                    });
                    if let Some(sig) = signature {
                        events.push(IrStreamEvent::BlockDelta {
                            index,
                            delta: IrDelta::SignatureDelta(sig.clone()),
                        });
                    }
                }
                events.push(IrStreamEvent::BlockStop { index });
                index += 1;
            }
            // Generated media rides the stream after the other blocks (IR-21, below).
            IrBlock::Image { .. } | IrBlock::Media { .. } => {}
            // No streamed analog: skipped without spending an index. Listed explicitly so a new block
            // kind fails to compile.
            IrBlock::ToolResult { .. } | IrBlock::Json(_) | IrBlock::HostedToolRecord { .. } => {}
        }
    }
    // IR-21: a generated image / audio part (a Bedrock `image`, a Gemini `inlineData`) streams whole
    // as `MediaDelta` in its own block, trailing the other blocks so a dialect with no streamed
    // media still sees contiguous indices.
    for block in ir.content.iter() {
        if matches!(block, IrBlock::Image { .. } | IrBlock::Media { .. }) {
            events.push(IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Image,
                refusal: false,
            });
            events.push(IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::MediaDelta(Box::new(block.clone())),
            });
            events.push(IrStreamEvent::BlockStop { index });
            index += 1;
        }
    }
    // A completion that called a tool stops for `tool_use` when the upstream named no reason.
    let default_stop = if ir
        .content
        .iter()
        .any(|b| matches!(b, IrBlock::ToolUse { .. }))
    {
        crate::codec::ir::IrStopReason::ToolUse
    } else {
        crate::codec::ir::IrStopReason::EndTurn
    };
    events.push(IrStreamEvent::MessageDelta {
        stop_reason: Some(ir.stop_reason.unwrap_or(default_stop)),
        stop_sequence: ir.stop_sequence.clone(),
        usage: ir.usage.clone(),
        stop_detail: ir.stop_detail.clone(),
    });
    events.push(IrStreamEvent::MessageStop);
    events
}

/// Registry-resolved streaming-translator factory, the single seam both the hot (`engine/mod.rs`)
/// and degraded (`engine/walk.rs`) forward paths call so their wiring cannot drift. Same-protocol
/// builds the verbatim same-proto translator (byte-exact re-emit plus IR usage A-tap); cross-protocol
/// the reframing one. `None` (raw passthrough) when the response is not an incremental stream
/// (`!is_sse`) or the protocol is unknown. Its writer is bounded at the plane's `translate_cap`
/// ([`StreamTranslate::with_translate_cap`]).
pub fn new_stream_translator_on(
    ingress: &str,
    egress: &str,
    is_sse: bool,
    translate_cap: usize,
) -> Option<StreamTranslate> {
    if !is_sse {
        // A buffered body: cross-protocol never reaches here (the forward path buffers and
        // translates first); same-protocol is a verbatim relay of the far end's bytes.
        return None;
    }
    let st = if ingress == egress {
        StreamTranslate::new_same_proto(ingress)
    } else {
        StreamTranslate::new(ingress, egress)
    }?;
    Some(st.with_translate_cap(translate_cap))
}

/// Write a same-protocol OpenAI SSE frame to `out` with its top-level `usage` member removed,
/// preserving every other byte. `frame` is the original complete frame (with `data:` line and
/// terminator); `data_str` is the JSON payload `parse_sse_frame` read from it.
///
/// Fast path: a byte-level splice. [`crate::codec::dialect::top_level_usage_cut`] locates `usage`
/// (and one adjacent comma) without a DOM round-trip, and since an OpenAI bare-`data:` chunk's JSON
/// is a single-line verbatim substring, the frame is written around that range.
///
/// Fallback (rare, e.g. a multi-`data:`-line frame): reframe as a bare `data:` frame with the
/// stripped payload, preserving JSON key order and the original terminator (CRLF vs LF), differing
/// from a direct stream only by the missing key. If the scanner cannot classify the payload, the
/// original bytes are written unchanged. No DOM reserialize: `serde_json` is built without
/// `preserve_order`, so it would reorder keys, the tell this strip removes. The stream is never damaged.
fn rewrite_frame_strip_usage(out: &mut Vec<u8>, frame: &[u8], data_str: &str) {
    let Some((cut_start, cut_end)) = crate::codec::dialect::top_level_usage_cut(data_str) else {
        out.extend_from_slice(frame);
        return;
    };
    // The JSON appears verbatim and unique in a bare `data: {json}\n\n` frame, so a single-substring
    // find is exact. A second occurrence means the text also lives elsewhere: reframe.
    if let Ok(frame_str) = std::str::from_utf8(frame) {
        if let Some(pos) = frame_str.find(data_str) {
            if !frame_str[pos + data_str.len()..].contains(data_str) {
                out.extend_from_slice(&frame[..pos + cut_start]);
                out.extend_from_slice(&frame[pos + cut_end..]);
                return;
            }
        }
    }
    // The original terminator, so the reframed frame matches a direct stream (CRLF is not downgraded to LF).
    let terminator: &[u8] = if frame.ends_with(b"\r\n\r\n") {
        b"\r\n\r\n"
    } else if frame.ends_with(b"\n\n") {
        b"\n\n"
    } else if frame.ends_with(b"\r\n") {
        b"\r\n"
    } else if frame.ends_with(b"\n") {
        b"\n"
    } else {
        b"\n\n"
    };
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&data_str.as_bytes()[..cut_start]);
    out.extend_from_slice(&data_str.as_bytes()[cut_end..]);
    out.extend_from_slice(terminator);
}

#[cfg(test)]
#[path = "tests/rewrite_frame_strip_usage_tests.rs"]
mod rewrite_frame_strip_usage_tests;

/// Merge a trailing usage-only chunk's counts into the deferred terminal usage (SSE terminal-usage
/// fold), by the `last_usage` rule: a non-zero/`Some` trailing field overrides the terminal value,
/// a zero/`None` one leaves it intact.
/// Restore onto a terminal `MessageDelta`'s usage the input/cache fields the egress protocol only
/// reports at stream start (Anthropic), else the ingress writer and billing see a zero prompt-token
/// count. Fills only fields the delta left empty, so protocols carrying input on the terminal delta
/// (OpenAI `include_usage`, Gemini, Bedrock, Cohere) are never overwritten.
///
/// Shared by the cross-protocol path and the same-protocol side channel so they cannot disagree.
fn backfill_from_start_usage(
    usage: &mut crate::codec::ir::IrUsage,
    start: &crate::codec::ir::IrUsage,
) {
    if usage.input_tokens == 0 {
        usage.input_tokens = start.input_tokens;
    }
    if usage.cache_creation_input_tokens.is_none() {
        usage.cache_creation_input_tokens = start.cache_creation_input_tokens;
    }
    if usage.cache_read_input_tokens.is_none() {
        usage.cache_read_input_tokens = start.cache_read_input_tokens;
    }
    // The cache-tier sub-buckets use the same fill-if-absent rule as `cache_creation_input_tokens`:
    // restoring the parent but not the tiers would make the split irreconcilable.
    if usage.detail.cache_creation_5m_input_tokens.is_none() {
        usage.detail.cache_creation_5m_input_tokens = start.detail.cache_creation_5m_input_tokens;
    }
    if usage.detail.cache_creation_1h_input_tokens.is_none() {
        usage.detail.cache_creation_1h_input_tokens = start.detail.cache_creation_1h_input_tokens;
    }
}

fn merge_trailing_usage(acc: &mut crate::codec::ir::IrUsage, trailing: &crate::codec::ir::IrUsage) {
    if trailing.input_tokens > 0 {
        acc.input_tokens = trailing.input_tokens;
    }
    if trailing.output_tokens > 0 {
        acc.output_tokens = trailing.output_tokens;
    }
    if trailing.cache_creation_input_tokens.is_some() {
        acc.cache_creation_input_tokens = trailing.cache_creation_input_tokens;
    }
    if trailing.cache_read_input_tokens.is_some() {
        acc.cache_read_input_tokens = trailing.cache_read_input_tokens;
    }
    merge_trailing_usage_detail(&mut acc.detail, &trailing.detail);
    // The stated-total check belongs to the counters it was computed from. A frame carrying
    // counters states them cumulatively (Gemini restates every counter per usage-bearing chunk), so
    // its note, Some or None, checks the counts the fold now holds. A stale gap a later frame
    // reconciled must go, since its remainder is the billed open class `unitemized_tokens` and would
    // bill tokens the provider's final total does not state. A counter-less frame checked nothing
    // and leaves the note as it was.
    let counted = trailing.input_tokens > 0
        || trailing.output_tokens > 0
        || trailing.cache_creation_input_tokens.is_some()
        || trailing.cache_read_input_tokens.is_some();
    if counted {
        acc.detail.usage_identity_note = trailing.detail.usage_identity_note.clone();
    }
}

/// The detail half of the fold: every [`crate::codec::ir::IrUsageDetail`] bucket rides the same
/// Some-wins rule. Without it a streamed `/v1/responses` call over an OpenAI-chat reasoning egress
/// answered `reasoning_tokens: 0` where the buffered call had the real value, since the trailing
/// usage-only chunk is the only place those egresses report the sub-buckets.
///
/// The trailing detail is destructured with no `..` rest pattern on purpose: each new per-dialect
/// attribution field that landed on the struct without landing here reopened the stream-versus-
/// buffered hole for its dialect, so the next one must be a compile error here.
fn merge_trailing_usage_detail(
    acc: &mut crate::codec::ir::IrUsageDetail,
    trailing: &crate::codec::ir::IrUsageDetail,
) {
    let crate::codec::ir::IrUsageDetail {
        reasoning_tokens,
        cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens,
        search_units,
        web_search_requests,
        web_fetch_requests,
        service_tier,
        input_audio_tokens,
        output_audio_tokens,
        accepted_prediction_tokens,
        rejected_prediction_tokens,
        tool_use_prompt_tokens,
        billed_input_tokens,
        billed_output_tokens,
        billed_classifications,
        usage_identity_note,
        open_units,
        residual_units,
        traffic_type,
        create_time,
        by_modality,
    } = trailing;

    if reasoning_tokens.is_some() {
        acc.reasoning_tokens = *reasoning_tokens;
    }
    if cache_creation_5m_input_tokens.is_some() {
        acc.cache_creation_5m_input_tokens = *cache_creation_5m_input_tokens;
    }
    if cache_creation_1h_input_tokens.is_some() {
        acc.cache_creation_1h_input_tokens = *cache_creation_1h_input_tokens;
    }
    if search_units.is_some() {
        acc.search_units = *search_units;
    }
    // Anthropic's web-search count and served tier are attribution invisible in a reconciling token
    // total, so an unmerged one is a silent loss.
    if web_search_requests.is_some() {
        acc.web_search_requests = *web_search_requests;
    }
    // The fetch count rides the same Anthropic `message_delta` frames; it is the billed open class `web_fetch_requests`.
    if web_fetch_requests.is_some() {
        acc.web_fetch_requests = *web_fetch_requests;
    }
    if service_tier.is_some() {
        acc.service_tier = service_tier.clone();
    }
    // The by-modality split (presentation only) rides the same Some-wins rule.
    if by_modality.is_some() {
        acc.by_modality = by_modality.clone();
    }
    // The four OpenAI-family slices arrive on the trailing `include_usage` chunk, the only frame of
    // an OpenAI stream with a usage object, so they would read absent at `stream:true` and real at `stream:false`.
    if input_audio_tokens.is_some() {
        acc.input_audio_tokens = *input_audio_tokens;
    }
    if output_audio_tokens.is_some() {
        acc.output_audio_tokens = *output_audio_tokens;
    }
    if accepted_prediction_tokens.is_some() {
        acc.accepted_prediction_tokens = *accepted_prediction_tokens;
    }
    if rejected_prediction_tokens.is_some() {
        acc.rejected_prediction_tokens = *rejected_prediction_tokens;
    }
    // Gemini reports `toolUsePromptTokenCount` only on the trailing usage-bearing chunk.
    if tool_use_prompt_tokens.is_some() {
        acc.tool_use_prompt_tokens = *tool_use_prompt_tokens;
    }
    // Cohere `billed_units.*` ride the same rule: a streamed call reports them only on the terminal
    // `message-end.delta.usage`, else it under-reports vs buffered.
    if billed_input_tokens.is_some() {
        acc.billed_input_tokens = *billed_input_tokens;
    }
    if billed_output_tokens.is_some() {
        acc.billed_output_tokens = *billed_output_tokens;
    }
    // The usage-identity cross-check comes from whichever frame carried the counters, on Gemini the
    // trailing one (early chunks have a `usageMetadata` with no counters). Same-wins, so a trailing
    // discrepancy is reported as it would be buffered.
    if usage_identity_note.is_some() {
        acc.usage_identity_note = usage_identity_note.clone();
    }
    if billed_classifications.is_some() {
        acc.billed_classifications = *billed_classifications;
    }
    // Open-class counts a reader carried (Bedrock guardrail policy units) arrive only on the
    // `metadata` frame; dropping them would bill the streamed turn less than its buffered twin.
    if !open_units.is_empty() {
        acc.open_units = open_units.clone();
    }
    // Residuals (never billed) ride the frame that reported them likewise.
    if !residual_units.is_empty() {
        acc.residual_units = residual_units.clone();
    }
    // Gemini `usageMetadata.trafficType` arrives only on the trailing chunk (like
    // `tool_use_prompt_tokens`); dropping it loses the billing-lane attribution vs buffered.
    if traffic_type.is_some() {
        acc.traffic_type = traffic_type.clone();
    }
    // Like `traffic_type`: Gemini's top-level `createTime` rides the trailing usage-bearing chunk.
    if create_time.is_some() {
        acc.create_time = create_time.clone();
    }
}

#[cfg(test)]
#[path = "tests/merge_trailing_usage_tests.rs"]
mod merge_trailing_usage_tests;
