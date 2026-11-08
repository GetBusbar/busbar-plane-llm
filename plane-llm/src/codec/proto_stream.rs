// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Response-stream translation: the concrete `StreamTranslate` and its factory, driven by the
//! relay (`exchange::reply::relay`).

use crate::codec::dialect::{parse_sse_frame, write_sse_frame, SSE_DONE_FRAME, SSE_DONE_SENTINEL};
use crate::codec::framework::STREAM_ABORT_DETAIL;
use crate::codec::protocol::{find_frame_terminator, IrError};

use super::proto_codec::{protocol_for, Protocol, SameProtoEdit, StreamFraming, ToolIdRemap};
use std::borrow::Cow;

/// Pure cross-protocol response-stream translator. Feed egress-protocol SSE bytes, get ingress
/// SSE bytes: `egress.reader().read_response_events` (wire to IR, stateful fan-out) composed with
/// `ingress.writer().write_response_event` (IR to wire).
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
    /// eventstream frame (valid CRC32), not SSE.
    pub ingress_eventstream: bool,
    /// The host's monotonic reading (ns) when the first byte was fed, for a real `metrics.latencyMs`
    started_at: Option<u64>,
    /// Per-stream ingress-keyed framing state. The protocol-specific stream-shape decisions (OpenAI
    /// per-chunk identity replay and `include_usage` un-fold, Bedrock messageStop/metadata deferral
    /// and finish flush) live behind this vtable in the owning protocol's module
    /// ([`StreamFraming`]), so the translator names no wire quirk.
    framing: Box<dyn StreamFraming>,
    /// Cross-protocol tool-id native remap: reshapes each egress `tool_use` id (OpenAI `call_…`) to
    /// the ingress client's native shape (Anthropic `toolu_…`) so a foreign id never reaches the
    /// client.
    tool_id_remap: ToolIdRemap,
    /// The events fed to `translate_ir_event` come from an answer that already went through the
    /// ingress preparation pass, so their tool ids are in the client's shape and must not be
    /// reshaped again.
    ir_already_prepared: bool,
    /// Input-token usage captured at `MessageStart.usage`, carried to the terminal `MessageDelta`.
    start_usage: Option<crate::codec::ir::IrUsage>,
    /// Set once a terminal `MessageStop` was emitted. Guards against a trailing `MessageDelta`
    message_stopped: bool,
    /// Same-protocol universal-translate mode (Change B step 2): ingress == egress, built via
    /// [`StreamTranslate::new_same_proto`].
    same_proto: bool,
    /// The terminal IR-derived usage (Change A "A-tap"), accumulated from the
    /// `MessageStart`/`MessageDelta`/`MessageStop` events after the Anthropic start-usage backfill,
    /// so it has the real prompt and completion counts for every protocol.
    last_usage: Option<crate::codec::ir::IrUsage>,
    /// A genuine terminal error seen mid-stream, set when a reader emits an
    /// [`crate::codec::ir::IrStreamEvent::Error`] (Anthropic `error`, OpenAI in-band `{"error":...}`,
    /// Responses `response.failed`, Bedrock `*Exception`).
    terminal_error: Option<String>,
    /// Stop writing after a local encoding failure while continuing to read upstream usage.
    encoding_failed: bool,
    /// An upstream terminal error retains its refund semantics even if encoding also fails.
    upstream_error: bool,
    /// Terminal-usage fold (SSE ingress: anthropic/gemini/cohere/responses; see
    /// `StreamFraming::folds_terminal_usage`).
    pending_terminal: Option<(
        crate::codec::ir::IrStopReason,
        Option<String>,
        crate::codec::ir::IrUsage,
        Option<crate::codec::ir::IrStopDetail>,
    )>,
    pending_stop: bool,
    /// IR block indices whose BlockStart was translated and BlockStop has not. Anything left at the
    /// terminal frame was never closed by the egress reader (truncated upstream, or a terminal branch
    /// that does not drain every block, e.g. cohere `message-end` closes only text).
    open_blocks: std::collections::BTreeMap<usize, bool>,
    /// Test-only: counts frames that reached the `crate::codec::json::parse_str` DOM parse in the
    /// SSE loop, proving the same-proto read probe elides the parse for frames with no usage or
    /// failure.
    #[cfg(test)]
    pub decode_calls: std::cell::Cell<usize>,
}

impl StreamTranslate {
    /// Build a translator for an ingress to egress pair. `None` if either protocol is unknown or
    /// ingress == egress (the caller does native passthrough).
    pub fn new(ingress: &str, egress: &str) -> Option<Self> {
        if ingress == egress {
            return None;
        }
        Self::build(ingress, egress, false)
    }

    /// Build a same-protocol universal translator (Change B step 2). Unlike [`new`] it returns `Some`
    pub fn new_same_proto(proto: &str) -> Option<Self> {
        Self::build(proto, proto, true)
    }

    /// Shared body of [`new`] and [`new_same_proto`]; `same_proto` selects the verbatim re-emit path in `feed`.
    fn build(ingress: &str, egress: &str, same_proto: bool) -> Option<Self> {
        let ingress_proto = protocol_for(ingress)?;
        let egress_proto = protocol_for(egress)?;

        let emit_done = ingress_proto
            .decl()
            .is_some_and(|d| d.emits_sse_done_terminator);
        let ingress_eventstream = ingress_proto
            .decl()
            .is_some_and(|d| d.ingress_is_eventstream);
        let egress_eventstream = egress_proto
            .decl()
            .is_some_and(|d| d.ingress_is_eventstream);

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

            ir_already_prepared: false,
            start_usage: None,
            message_stopped: false,
            same_proto,
            last_usage: None,
            terminal_error: None,
            encoding_failed: false,
            upstream_error: false,
            pending_terminal: None,
            pending_stop: false,
            open_blocks: std::collections::BTreeMap::new(),
            #[cfg(test)]
            decode_calls: std::cell::Cell::new(0),
        })
    }

    /// The translator whose ingress writer bounds each per-item accumulator at `cap`: the
    /// translation cap the plane enforces (`limits.request_body_max_bytes`).
    #[must_use]
    pub fn with_translate_cap(self, cap: usize) -> Self {
        self.ingress.writer().set_translate_cap(cap);
        self
    }

    /// Record whether the client opted into streaming usage (`stream_options.include_usage ==
    /// true`) and forward it to the ingress framing.
    pub fn set_client_include_usage(&mut self, include: bool) {
        self.framing.set_client_include_usage(include);
    }

    /// Forward the original ingress request body to the ingress writer for the stream's life.
    pub fn set_request_echo(&mut self, ingress_request_body: &serde_json::Value) {
        self.ingress.writer().set_request_echo(ingress_request_body);
    }

    /// Translate one egress event `(event_type, payload)` into ingress wire bytes, advancing the
    /// decode state.
    fn translate_event(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        out: &mut Vec<u8>,
    ) {
        let ingress_name = self.ingress.name_static();

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
        self.recover_unread_stream_usage(event_type, data, &events);
        let failed_before_frame = self.upstream_error;
        for ev in events {
            if !failed_before_frame
                && self.upstream_error
                && !self.message_stopped
                && matches!(ev, crate::codec::ir::IrStreamEvent::MessageStop)
            {
                self.message_stopped = true;
                self.emit_ir_event(host, &ev, out);
            } else {
                self.translate_ir_event(host, ingress_name, ev, out);
            }
        }
    }

    /// Far-end wire paths this stream dropped on its way to the caller, each warned once; the host
    /// records one audit row per path at stream end.
    pub fn dropped(&self) -> &[String] {
        &self.decode.dropped
    }

    /// Run one decoded IR event through the cross-protocol pipeline (tool-id remap, identity strip,
    /// usage backfill and A-tap, framing seams, ingress writer) and append the framed bytes to
    /// `out`.
    fn translate_ir_event(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        ingress_name: &'static str,
        mut ev: crate::codec::ir::IrStreamEvent,
        out: &mut Vec<u8>,
    ) {
        if !self.ir_already_prepared {
            self.tool_id_remap.remap_event(ingress_name, &mut ev);

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

        if let crate::codec::ir::IrStreamEvent::MessageStart {
            id, created, usage, ..
        } = &mut ev
        {
            if let Some(u) = usage {
                self.start_usage = Some(u.clone());

                self.fold_reported_usage(u);
            }
            *id = None;
            *created = None;
        }

        if let crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } = &mut ev {
            if let Some(start) = &self.start_usage {
                backfill_from_start_usage(usage, start);
            }

            self.fold_reported_usage(usage);
        }

        if self.upstream_error {
            return;
        }

        if let crate::codec::ir::IrStreamEvent::Error(err) = &ev {
            self.close_failed_blocks(host, out);
            self.upstream_error = true;
            self.pending_terminal = None;
            self.pending_stop = false;
            self.terminal_error = Some(
                err.provider_signal
                    .clone()
                    .unwrap_or_else(|| "upstream stream error".to_string()),
            );
        }

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

        if matches!(
            ev,
            crate::codec::ir::IrStreamEvent::MessageDelta {
                stop_reason: Some(_),
                ..
            } | crate::codec::ir::IrStreamEvent::MessageStop
        ) {
            self.close_open_blocks(host, out);
        }

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

        if self.framing.folds_terminal_usage() {
            match &ev {
                crate::codec::ir::IrStreamEvent::MessageDelta {
                    stop_reason: Some(reason),
                    usage,
                    stop_sequence,
                    stop_detail,
                } => {
                    match self.pending_terminal.as_mut() {
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

        if self.message_stopped
            && matches!(ev, crate::codec::ir::IrStreamEvent::MessageDelta { .. })
        {
            return;
        }
        if matches!(ev, crate::codec::ir::IrStreamEvent::MessageStop) {
            self.message_stopped = true;
        }

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

        if is_message_start && !self.encoding_failed {
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
    pub fn synthesize_from_response(
        host: &dyn crate::codec::cells::CodecHost,
        ingress: &str,
        ir: &crate::codec::ir::IrResponse,
        translate_cap: usize,
    ) -> Option<Vec<u8>> {
        let mut st = Self::build(ingress, ingress, false)?.with_translate_cap(translate_cap);
        if st.ingress_eventstream {
            return None;
        }

        if let Some(request) = ir.request_echo.as_ref() {
            st.set_request_echo(request);
            st.set_client_include_usage(
                request
                    .pointer("/stream_options/include_usage")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            );
        }

        st.ir_already_prepared = true;
        let (events, origins) = response_to_ir_events_with_origins(ir);
        st.decode.block_origins = origins;
        let ingress_name = st.ingress.name_static();
        let mut out: Vec<u8> = Vec::new();
        for ev in events {
            st.translate_ir_event(host, ingress_name, ev, &mut out);
        }
        out.extend_from_slice(&st.finish(host));
        Some(out)
    }

    /// Fold a far-end-reported usage into `last_usage`, the billing source: the one per-field
    /// non-zero/`Some`-wins merge (shared with the terminal-usage fold), so the cross- and
    /// same-protocol A-taps cannot drift and the detail buckets ride along.
    fn fold_reported_usage(&mut self, reported: &crate::codec::ir::IrUsage) {
        let acc = self
            .last_usage
            .get_or_insert(crate::codec::ir::IrUsage::default());
        merge_trailing_usage(acc, reported);
    }

    /// Recover accounting only when the content reader could not carry a report. A normal
    /// usage-bearing IR event keeps its existing read, warning, backfill and writer path.
    fn recover_unread_stream_usage(
        &mut self,
        event_type: &str,
        data: &serde_json::Value,
        events: &[crate::codec::ir::IrStreamEvent],
    ) {
        if events.iter().any(|event| {
            matches!(
                event,
                crate::codec::ir::IrStreamEvent::MessageStart { usage: Some(_), .. }
                    | crate::codec::ir::IrStreamEvent::MessageDelta { .. }
            )
        }) {
            return;
        }
        match self.egress.reader().recover_stream_usage(event_type, data) {
            Some(super::proto_codec::ReportedStreamUsage::Start(usage)) => {
                self.start_usage = Some(usage.clone());
                self.fold_reported_usage(&usage);
            }
            Some(super::proto_codec::ReportedStreamUsage::Trailing(mut usage)) => {
                if let Some(start) = &self.start_usage {
                    backfill_from_start_usage(&mut usage, start);
                }
                self.fold_reported_usage(&usage);
            }
            None => {}
        }
    }

    /// Same-protocol usage side-channel (Change B step 2): runs only the egress reader and the
    /// start-usage/backfill/`last_usage` accumulation of `translate_event`, with no remap, identity
    /// strip, writer or reframe, since the original bytes re-emit verbatim.
    fn extract_usage_only(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
    ) {
        let events =
            self.egress
                .reader()
                .read_response_events(host, event_type, data, &mut self.decode);
        self.recover_unread_stream_usage(event_type, data, &events);
        for ev in events {
            if let crate::codec::ir::IrStreamEvent::MessageStart { usage: Some(u), .. } = &ev {
                self.start_usage = Some(u.clone());
                self.fold_reported_usage(u);
            }

            if let crate::codec::ir::IrStreamEvent::Error(err) = &ev {
                self.upstream_error = true;
                self.terminal_error = Some(
                    err.provider_signal
                        .clone()
                        .unwrap_or_else(|| "upstream stream error".to_string()),
                );
            }
            if let crate::codec::ir::IrStreamEvent::MessageDelta { usage, .. } = &ev {
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
        host: &dyn crate::codec::cells::CodecHost,
        ev: &crate::codec::ir::IrStreamEvent,
        out: &mut Vec<u8>,
    ) {
        if self.encoding_failed {
            return;
        }

        match ev {
            crate::codec::ir::IrStreamEvent::BlockStart { index, block, .. } => {
                self.open_blocks.insert(
                    *index,
                    matches!(block, crate::codec::ir::IrBlockMeta::ToolUse { .. }),
                );
            }
            crate::codec::ir::IrStreamEvent::BlockStop { index } => {
                self.open_blocks.remove(index);
            }
            _ => {}
        }

        let origin = match ev {
            crate::codec::ir::IrStreamEvent::BlockStart { index, .. }
            | crate::codec::ir::IrStreamEvent::BlockDelta { index, .. }
            | crate::codec::ir::IrStreamEvent::BlockStop { index } => {
                self.decode.block_origins.get(index).cloned()
            }
            _ => None,
        };
        let encoded = if self.ingress.name_static() == self.egress.name_static() {
            crate::codec::drops::with_block_origin(origin.as_ref(), || {
                self.ingress.writer().try_write_response_events(host, ev)
            })
        } else {
            let mut dropped = std::mem::take(&mut self.decode.dropped);
            let seam = crate::codec::drops::Seam {
                direction: crate::codec::drops::Direction::Response,
                ingress: self.ingress.name_static(),
                egress: self.egress.name_static(),
            };
            let encoded = crate::codec::drops::scope_more(seam, &mut dropped, || {
                crate::codec::drops::with_block_origin(origin.as_ref(), || {
                    self.ingress.writer().try_write_response_events(host, ev)
                })
            });
            self.decode.dropped = dropped;
            encoded
        };
        let frames = match encoded {
            Ok(frames) => frames,
            Err(error) => {
                self.encoding_failed = true;
                if self.terminal_error.is_none() {
                    self.terminal_error = error.provider_signal;
                }

                let error = if let crate::codec::ir::IrStreamEvent::Error(original) = ev {
                    self.ingress.writer().write_error(
                        host,
                        500,
                        "server_error",
                        original
                            .provider_signal
                            .as_deref()
                            .unwrap_or("upstream stream error"),
                    )
                } else {
                    self.ingress.writer().encoding_failure(host)
                };
                write_sse_frame(out, "error", &error);
                return;
            }
        };
        for (out_et, mut out_data) in frames {
            if self.ingress_eventstream {
                let elapsed_ms = self
                    .started_at
                    .zip(host.monotonic_ns())
                    .map(|(start, now)| now.saturating_sub(start) / 1_000_000);
                self.framing
                    .inject_streaming_metrics(&out_et, &mut out_data, elapsed_ms);
                let payload = crate::codec::json::to_vec(&out_data).unwrap_or_default();

                out.extend_from_slice(&crate::codec::eventstream::encode_frame(&out_et, &payload));
            } else {
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
    /// `emit_ir_event` calls cannot re-insert (idempotent).
    fn close_open_blocks(&mut self, host: &dyn crate::codec::cells::CodecHost, out: &mut Vec<u8>) {
        for (index, _) in std::mem::take(&mut self.open_blocks) {
            self.emit_ir_event(
                host,
                &crate::codec::ir::IrStreamEvent::BlockStop { index },
                out,
            );
        }
    }

    /// Close already-visible non-tool blocks on an upstream failure. Stopping a deferred tool
    /// can emit a runnable call, so discard those entries instead of completing them.
    fn close_failed_blocks(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        out: &mut Vec<u8>,
    ) {
        for (index, tool) in std::mem::take(&mut self.open_blocks) {
            if !tool {
                self.emit_ir_event(
                    host,
                    &crate::codec::ir::IrStreamEvent::BlockStop { index },
                    out,
                );
            }
        }
    }

    /// Hard cap on the reassembly buffer, so an upstream that never emits a frame terminator cannot
    /// exhaust memory.
    pub const MAX_BUF: usize = crate::codec::eventstream::MAX_FRAME_BYTES;

    /// Feed a chunk of egress SSE bytes; return translated ingress bytes for the complete frames now
    /// available (empty for a partial frame).
    #[cfg(test)]
    pub fn feed(&mut self, host: &dyn crate::codec::cells::CodecHost, chunk: &[u8]) -> Vec<u8> {
        self.feed_borrowed(host, chunk).into_owned()
    }

    /// [`Self::feed`], borrowing where it can: a same-protocol SSE piece on a frame boundary
    /// (nothing buffered) needing no edit is answered as `chunk[..consumed]` with no copy; only an
    /// unterminated tail is copied into the reassembly buffer.
    pub fn feed_borrowed<'a>(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        chunk: &'a [u8],
    ) -> Cow<'a, [u8]> {
        if self.aborted {
            return Cow::Borrowed(&[]);
        }

        if self.started_at.is_none() && self.ingress_eventstream {
            self.started_at = host.monotonic_ns();
        }
        if self.egress_eventstream {
            return Cow::Owned(self.feed_eventstream(host, chunk));
        }

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

        if self.aborted {
            return out;
        }
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
        host: &dyn crate::codec::cells::CodecHost,
        chunk: &[u8],
    ) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out: Vec<u8> = Vec::new();

        let (frames, status, _valid_consumed) = crate::codec::eventstream::drain_frames_signaled(
            &mut self.buf,
            if self.same_proto {
                Some(&mut out)
            } else {
                None
            },
        );
        for frame in frames {
            let crate::codec::eventstream::DecodedFrame {
                event_type,
                payload,
                error,
            } = frame;
            if let Some(error) = error {
                let err = IrError {
                    class: crate::codec::bedrock::stream_error_class(&event_type),
                    provider_signal: error
                        .message
                        .or(error.code)
                        .or_else(|| Some("AWS event-stream error".to_string())),
                    retry_after: None,
                };
                if self.same_proto {
                    self.upstream_error = true;
                    self.terminal_error = err.provider_signal;
                } else {
                    self.translate_ir_event(
                        host,
                        self.ingress.name_static(),
                        crate::codec::ir::IrStreamEvent::Error(err),
                        &mut out,
                    );
                }
                continue;
            }

            if !self.egress.reader().reads_eventstream_event(&event_type) {
                if !self.same_proto {
                    let seam = crate::codec::drops::Seam {
                        direction: crate::codec::drops::Direction::Response,
                        ingress: self.ingress.name_static(),
                        egress: self.egress.name_static(),
                    };
                    // The header alone names this loss; future payloads need not be JSON.
                    crate::codec::drops::scope_more(seam, &mut self.decode.dropped, || {
                        crate::codec::drops::note(crate::codec::drops::Dropped::new(
                            event_type,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            crate::codec::drops::UNMAPPED_ANSWER_MEMBER,
                        ));
                    });
                }
                continue;
            }
            if self.same_proto && !self.egress.reader().same_proto_reads(&event_type, &payload) {
                continue;
            }
            let Ok(mut data) = serde_json::from_slice::<serde_json::Value>(&payload) else {
                if !self.same_proto {
                    self.abort();
                    break;
                }
                continue;
            };
            if let Some(obj) = data.as_object_mut() {
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String(event_type.clone()),
                );
            }
            if self.same_proto {
                self.extract_usage_only(host, &event_type, &data);
            } else {
                self.translate_event(host, &event_type, &data, &mut out);
            }
        }

        if status == crate::codec::eventstream::DrainStatus::MalformedPrelude
            || self.buf.len() > Self::MAX_BUF
        {
            self.abort();
        }
        out
    }

    /// Drain every complete blank-line-delimited SSE frame of `src` (the buffer, or a piece on a
    /// frame boundary).
    fn drain_sse(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        src: &[u8],
    ) -> (usize, Option<Vec<u8>>) {
        let mut consumed = 0usize;
        let mut out: Vec<u8> = Vec::new();

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
                continue;
            };
            if data.is_empty() || data == SSE_DONE_SENTINEL {
                continue;
            }
            if !self.same_proto {
                #[cfg(test)]
                self.decode_calls.set(self.decode_calls.get() + 1);
                let Ok(value) = crate::codec::json::parse_str::<serde_json::Value>(&data) else {
                    self.abort();
                    break;
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
                    continue;
                };
                self.extract_usage_only(host, event_type, &value);
            }

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
    /// no terminator, a malformed egress event-stream prelude, or invalid translated JSON), so the happy-path terminal events
    /// were never produced and further `feed`s are no-ops.
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// The terminal IR-derived usage (Change A "A-tap"), or `None` if no usage-bearing terminal event
    /// was seen.
    pub fn usage(&self) -> Option<&crate::codec::ir::IrUsage> {
        self.last_usage.as_ref()
    }

    /// The terminal stream error message, or `None` for a clean stream (Change A). The
    /// `FirstByteBody` stream-end arm reads it for breaker disposition (non-`None` records a breaker
    /// transient and suppresses token billing).
    pub fn terminal_error(&self) -> Option<&str> {
        self.terminal_error.as_deref()
    }

    /// A local encoding failure is a translation abort that retains reported work. An upstream
    /// terminal error keeps the reply's existing refund semantics instead.
    pub fn local_encoding_failure(&self) -> bool {
        self.encoding_failed && !self.upstream_error
    }

    /// Frame a terminal mid-stream error through the live ingress writer this translator has driven
    /// since the first frame, the one that latched the stream's identity.
    pub fn terminal_error_frame(
        &mut self,
        host: &dyn crate::codec::cells::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        self.ingress.writer().write_error_frame(host, err)
    }

    /// Abandon the stream as unrecoverable: release the reassembly buffer, set `aborted` so further
    /// `feed()` is a no-op, and let `finish()` emit the ingress-native terminal error frame.
    fn abort(&mut self) {
        self.aborted = true;
        self.buf.clear();
        self.buf.shrink_to_fit();
        self.scanned = 0;
    }

    /// Call once at end-of-stream. Returns the ingress terminator (OpenAI: `data: [DONE]\n\n`;
    /// Anthropic: empty, `message_stop` already terminates).
    pub fn finish(&mut self, host: &dyn crate::codec::cells::CodecHost) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();

        if !self.same_proto && (self.upstream_error || self.encoding_failed) {
            if self.emit_done {
                out.extend_from_slice(SSE_DONE_FRAME);
            }
            return out;
        }

        if self.aborted {
            const ABORT_DETAIL: &str = STREAM_ABORT_DETAIL;
            if let Some(exc_type) = self.framing.abort_exception_type() {
                out.extend_from_slice(&crate::codec::eventstream::encode_exception_frame(
                    exc_type,
                    ABORT_DETAIL,
                ));
                return out;
            }

            let err = IrError {
                class: busbar_contract::upstream::StatusClass::ServerError,
                provider_signal: Some(ABORT_DETAIL.to_string()),
                retry_after: None,
            };
            let ev = crate::codec::ir::IrStreamEvent::Error(err);
            self.emit_ir_event(host, &ev, &mut out);

            if self.emit_done {
                out.extend_from_slice(SSE_DONE_FRAME);
            }
            return out;
        }

        if self.same_proto {
            return out;
        }

        self.close_open_blocks(host, &mut out);

        if let Some(trailing) = self.framing.on_finish() {
            self.emit_ir_event(host, &trailing, &mut out);
        }

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
/// `MessageDelta` and `MessageStop`, so the ingress writer sees the live-path shape.
#[cfg(test)]
pub fn response_to_ir_events(
    ir: &crate::codec::ir::IrResponse,
) -> Vec<crate::codec::ir::IrStreamEvent> {
    response_to_ir_events_with_origins(ir).0
}

fn response_to_ir_events_with_origins(
    ir: &crate::codec::ir::IrResponse,
) -> (
    Vec<crate::codec::ir::IrStreamEvent>,
    std::collections::BTreeMap<usize, crate::codec::ir::IrBlockOrigin>,
) {
    use crate::codec::ir::{IrBlock, IrBlockMeta, IrDelta, IrStreamEvent};
    let mut events: Vec<IrStreamEvent> = Vec::new();
    let mut origins = std::collections::BTreeMap::new();
    if ir.audio.is_some() {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::wire("choices[].message.audio"),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [],
            "dropping native Chat audio during stream synthesis: no streamed IR audio-output carrier"
        );
    }
    events.push(IrStreamEvent::MessageStart {
        role: ir.role,

        usage: None,
        id: ir.id.clone(),
        created: ir.created,
        model: ir.model.clone(),
    });
    let mut index = 0usize;
    let mut logprobs_pending = !ir.logprobs.is_empty();
    for (source_index, block) in ir.content.iter().enumerate() {
        match block {
            IrBlock::Text {
                text,
                citations,
                refusal,
                ..
            } => {
                if let Some(origin) = ir.block_origins.response.get(source_index) {
                    origins.insert(index, origin.clone());
                }
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
                if let Some(origin) = ir.block_origins.response.get(source_index) {
                    origins.insert(index, origin.clone());
                }
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
                    delta: IrDelta::InputJsonDelta(
                        crate::codec::dialect::tool_arguments_to_string(input),
                    ),
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
                if let Some(origin) = ir.block_origins.response.get(source_index) {
                    origins.insert(index, origin.clone());
                }
                events.push(IrStreamEvent::BlockStart {
                    index,
                    block: if *redacted {
                        IrBlockMeta::RedactedThinking
                    } else {
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

            IrBlock::Image { .. } | IrBlock::Media { .. } => {}

            block @ (IrBlock::ToolResult { .. }
            | IrBlock::Json(_)
            | IrBlock::HostedToolRecord { .. }) => {
                let _origin =
                    crate::codec::drops::block_scope(ir.block_origins.response.get(source_index));
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::block(block.kind_name()),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping an answer block during stream synthesis: no streamed IR analogue"
                );
            }
        }
    }

    for (source_index, block) in ir.content.iter().enumerate() {
        if matches!(block, IrBlock::Image { .. } | IrBlock::Media { .. }) {
            if let Some(origin) = ir.block_origins.response.get(source_index) {
                origins.insert(index, origin.clone());
            }
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
    (events, origins)
}

/// Registry-resolved streaming-translator factory, the single seam both the hot (`engine/mod.rs`)
/// and degraded (`engine/walk.rs`) forward paths call so their wiring cannot drift.
pub fn new_stream_translator_on(
    ingress: &str,
    egress: &str,
    is_sse: bool,
    translate_cap: usize,
) -> Option<StreamTranslate> {
    if !is_sse {
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
/// preserving every other byte.
fn rewrite_frame_strip_usage(out: &mut Vec<u8>, frame: &[u8], data_str: &str) {
    let Some((cut_start, cut_end)) = crate::codec::dialect::top_level_usage_cut(data_str) else {
        out.extend_from_slice(frame);
        return;
    };

    if let Ok(frame_str) = std::str::from_utf8(frame) {
        if let Some(pos) = frame_str.find(data_str) {
            if !frame_str[pos + data_str.len()..].contains(data_str) {
                out.extend_from_slice(&frame[..pos + cut_start]);
                out.extend_from_slice(&frame[pos + cut_end..]);
                return;
            }
        }
    }

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
fn backfill_from_start_usage(
    usage: &mut crate::codec::ir::IrUsage,
    start: &crate::codec::ir::IrUsage,
) {
    if let Some(start_exact) = &start.detail.exact_counts {
        let exact = usage
            .detail
            .exact_counts
            .get_or_insert_with(Default::default);
        for (class, count) in start_exact {
            exact.entry(class.clone()).or_insert(*count);
        }
    }
    usage
        .detail
        .refused_counts
        .extend(start.detail.refused_counts.iter().cloned());
    if usage.input_tokens == 0 {
        usage.input_tokens = start.input_tokens;
    }
    if usage.cache_creation_input_tokens.is_none() {
        usage.cache_creation_input_tokens = start.cache_creation_input_tokens;
    }
    if usage.cache_read_input_tokens.is_none() {
        usage.cache_read_input_tokens = start.cache_read_input_tokens;
    }

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

    let counted = trailing.input_tokens > 0
        || trailing.output_tokens > 0
        || trailing.cache_creation_input_tokens.is_some()
        || trailing.cache_read_input_tokens.is_some();
    if counted {
        acc.detail.usage_identity_note = trailing.detail.usage_identity_note.clone();
    }
}

/// The detail half of the fold: every [`crate::codec::ir::IrUsageDetail`] bucket rides the same
/// Some-wins rule.
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
        exact_counts,
        refused_counts,
        exact_tool_use_prompt_tokens,
    } = trailing;

    if exact_tool_use_prompt_tokens.is_some() {
        acc.exact_tool_use_prompt_tokens = *exact_tool_use_prompt_tokens;
    }
    if let Some(exact) = exact_counts {
        acc.exact_counts
            .get_or_insert_with(Default::default)
            .extend(exact.clone());
    }
    acc.refused_counts.extend(refused_counts.iter().cloned());

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

    if web_search_requests.is_some() {
        acc.web_search_requests = *web_search_requests;
    }

    if web_fetch_requests.is_some() {
        acc.web_fetch_requests = *web_fetch_requests;
    }
    if service_tier.is_some() {
        acc.service_tier = service_tier.clone();
    }

    if by_modality.is_some() {
        acc.by_modality = by_modality.clone();
    }

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

    if tool_use_prompt_tokens.is_some() {
        acc.tool_use_prompt_tokens = *tool_use_prompt_tokens;
    }

    if billed_input_tokens.is_some() {
        acc.billed_input_tokens = *billed_input_tokens;
    }
    if billed_output_tokens.is_some() {
        acc.billed_output_tokens = *billed_output_tokens;
    }

    if usage_identity_note.is_some() {
        acc.usage_identity_note = usage_identity_note.clone();
    }
    if billed_classifications.is_some() {
        acc.billed_classifications = *billed_classifications;
    }

    if !open_units.is_empty() {
        acc.open_units = open_units.clone();
    }

    if !residual_units.is_empty() {
        acc.residual_units = residual_units.clone();
    }

    if traffic_type.is_some() {
        acc.traffic_type = traffic_type.clone();
    }

    if create_time.is_some() {
        acc.create_time = create_time.clone();
    }
}

#[cfg(test)]
#[path = "tests/merge_trailing_usage_tests.rs"]
mod merge_trailing_usage_tests;

#[cfg(test)]
#[path = "tests/block_origin_synthesis_tests.rs"]
mod block_origin_synthesis_tests;

#[cfg(test)]
#[path = "tests/audit_malformed_stream_tests.rs"]
mod audit_malformed_stream_tests;
