// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The chat `IrHandle`: the request/response handle `ChatOperation` yields. Wraps the concrete
//! `crate::codec::ir::{IrRequest,IrResponse}` and carries chat's cross-protocol prep and write,
//! keyed by the peer protocol string: the handle writes itself onto the target dialect via
//! `super::proto_codec::protocol_for(proto).writer()`, with no downcast.

use crate::codec::drops::{self, Dropped};
use crate::codec::ir::{IrReasoningAsk, IrRequest, IrResponse};
use busbar_contract::billing::Billing;
use busbar_contract::codec::{CodecError, IngressReject, OperationHandler};
use busbar_contract::codec::{EgressWire, TranslatedResponse};
use busbar_contract::ir::egress_prep::EgressPrep;
use busbar_contract::ir::facts::IrFacts;
use busbar_contract::ir::handle::sealed::Sealed;
use busbar_contract::ir::handle::IrHandle;
use busbar_contract::operation::OpVerb;
use busbar_contract::SlabBytes;
use serde_json::Value;

/// Gemini's server-side context-cache reference: a request member no other dialect can carry.
const CACHED_CONTENT: &str = "cachedContent";

/// Google's documented dummy `thoughtSignature` sentinel.
pub const GEMINI_SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// 1.5.5's operator warning, restored (TODO 585): an image's requested fidelity a dialect has no slot
/// for is dropped with a warn, once per image, naming the word. `auto` (the default) logs nothing.
pub fn warn_dropped_image_details(ir: &IrRequest) {
    fn walk(blocks: &[crate::codec::ir::IrBlock]) {
        for block in blocks {
            match block {
                crate::codec::ir::IrBlock::Image {
                    detail: Some(d), ..
                } if *d != crate::codec::ir::IrImageDetail::Auto => {
                    let detail = d.as_str();
                    ::tracing::warn!(
                        detail,
                        "dropping image_url.detail: no cross-protocol carrier exists for this \
                         cost/latency hint (not even on a same-protocol OpenAI round-trip)"
                    );
                }
                crate::codec::ir::IrBlock::ToolResult { content, .. } => walk(content),
                _ => {}
            }
        }
    }
    walk(&ir.system);
    for m in &ir.messages {
        walk(&m.content);
    }
}

/// Chat cross-protocol egress preparation.
pub fn chat_prepare_for_egress(ir: &mut IrRequest, prep: &EgressPrep) {
    if ir.max_tokens.is_none() && prep.egress_requires_max_tokens {
        ir.max_tokens = Some(
            prep.lane_default_max_tokens
                .unwrap_or(prep.global_default_max_tokens),
        );
    }
    super::proto_codec::decode_request_tool_ids(prep.ingress_protocol, &mut ir.messages);
    // n>1 clamp. `n` asks for N candidates but `IrResponse` carries one, so forwarding `n>1` made
    // the backend generate and bill N candidates while translation kept only choice 0: wasted spend
    // and a response not matching the request. Clamp `n` to 1 before the egress writer emits it. A
    // same-protocol passthrough never reaches here (verbatim body), so `n>1` still works there.
    if ir.n.is_some_and(|n| n > 1) {
        drops::note(Dropped::slot(
            "n",
            &crate::codec::diagnostics::IR_CLAMP_N_TO_1,
            "clamping n>1 to 1 on the cross-protocol seam: the neutral response IR carries \
             a single candidate, so extra choices would be generated, billed, and then \
             dropped; the backend is asked for exactly one candidate",
        ));
        ir.n = Some(1);
    }
    // The reasoning gate. A lane that did not claim the capability never receives a thinking param
    // (a non-reasoning model would 400); the request proceeds, thinking at the backend's default.
    if ir.reasoning.is_some() {
        if prep.reasoning_allowed {
            ir.reasoning_budgets = Some(prep.reasoning_budgets);
        } else {
            // Named by the caller's member the ask came from: effort rows for a word, budget rows for a token budget.
            let rows: &'static [&'static str] = match ir.reasoning {
                Some(IrReasoningAsk::Budget(_) | IrReasoningAsk::Dynamic) => &["thinking_budget"],
                _ => &["reasoning_effort", "reasoning"],
            };
            drops::note(
                Dropped::slot(
                    "reasoning",
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    "dropping cross-protocol reasoning/thinking ask: the target lane does \
                     not declare the capability; set `reasoning: true` on the model (or \
                     pool member) if this backend accepts thinking params",
                )
                .rows(rows),
            );
            ir.reasoning = None;
        }
    }
    // The prompt-cache gate, twin of the reasoning gate. Fires when the egress writer's cache marker
    // is model-gated (Bedrock `cachePoint`) and the lane did not assert `prompt_caching`: breakpoints
    // are cleared so the writer emits no marker a model like Amazon Nova would 400 on. The request
    // proceeds uncached (fail-safe over fail-hard).
    if !prep.prompt_caching_allowed {
        let mut cleared = false;
        let mut clear_blocks = |blocks: &mut [crate::codec::ir::IrBlock]| {
            for b in blocks {
                let cc = match b {
                    crate::codec::ir::IrBlock::Text { cache_control, .. }
                    | crate::codec::ir::IrBlock::Thinking { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolUse { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolResult { cache_control, .. }
                    | crate::codec::ir::IrBlock::Image { cache_control, .. }
                    | crate::codec::ir::IrBlock::Media { cache_control, .. } => cache_control,
                    // A raw JSON tool-result block carries no cache breakpoint.
                    crate::codec::ir::IrBlock::Json(_)
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. } => continue,
                };
                cleared |= cc.take().is_some();
            }
        };
        clear_blocks(&mut ir.system);
        for m in &mut ir.messages {
            clear_blocks(&mut m.content);
        }
        for t in &mut ir.tools {
            cleared |= t.cache_control.take().is_some();
        }
        if cleared {
            drops::note(Dropped::slot(
                "cache_control",
                &crate::codec::diagnostics::IR_DROP_PROMPT_CACHE,
                "dropping cross-protocol prompt-cache breakpoints: the target lane's \
                 dialect gates its cache marker per model and the lane does not \
                 declare the capability; set `prompt_caching: true` on the model if \
                 this backend accepts cache markers (e.g. Claude on Bedrock)",
            ));
        }
    }
    // Anthropic cache_control cap: "A maximum of 4 blocks with cache_control may be provided".
    // Reachable only cross-protocol (same-protocol is a verbatim passthrough, so it never clamps a
    // request legal for its own backend). Walk Anthropic's prefix order (system, every message's
    // content, every tool), `.take()`ing breakpoints past the cap, and warn once with the dropped count.
    if let Some(cap) = prep.cache_control_cap {
        let mut seen = 0usize;
        let mut dropped = 0usize;
        let mut walk_take = |blocks: &mut [crate::codec::ir::IrBlock]| {
            for b in blocks {
                let cc = match b {
                    crate::codec::ir::IrBlock::Text { cache_control, .. }
                    | crate::codec::ir::IrBlock::Thinking { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolUse { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolResult { cache_control, .. }
                    | crate::codec::ir::IrBlock::Image { cache_control, .. }
                    | crate::codec::ir::IrBlock::Media { cache_control, .. } => cache_control,
                    crate::codec::ir::IrBlock::Json(_)
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. } => continue,
                };
                if cc.is_some() {
                    if seen < cap {
                        seen += 1;
                    } else {
                        *cc = None;
                        dropped += 1;
                    }
                }
            }
        };
        walk_take(&mut ir.system);
        for m in &mut ir.messages {
            walk_take(&mut m.content);
        }
        for t in &mut ir.tools {
            if t.cache_control.is_some() {
                if seen < cap {
                    seen += 1;
                } else {
                    t.cache_control = None;
                    dropped += 1;
                }
            }
        }
        if dropped > 0 {
            drops::note(Dropped::slot(
                "cache_control",
                &crate::codec::diagnostics::IR_DROP_CACHE_CONTROL_OVER_CAP,
                format!(
                    "dropping {dropped} cache_control breakpoint(s) past the egress \
                     dialect's cap of {cap}: the target vendor 400s past this count and \
                     the IR carries breakpoints unbounded"
                ),
            ));
        }
    }
    // Hosted-tool cross-protocol drop of the raw, same-protocol-only `IrTool::hosted` object. A hosted
    // tool whose kind a reader recognises crosses in the typed `IrRequest::hosted_tools` slot (IR-11);
    // only the verbatim Responses object no other writer can emit is dropped here. A Responses hosted
    // tool (`{"type":"web_search"}` as `IrTool{ name:"", hosted:Some(..) }`) has no function-tool
    // analog and only the Responses writer honors `hosted`; others would emit a malformed
    // `{"type":"function","name":""}` the backend 400s. This runs only cross-protocol (Responses to
    // Responses is verbatim), so dropping every hosted tool keeps same-proto and drops cross-proto.
    // The count is kept for the warn before draining.
    for hosted in ir.tools.iter().filter_map(|t| t.hosted.as_ref()) {
        let kind = hosted
            .get(crate::codec::keys::TYPE)
            .and_then(Value::as_str)
            .unwrap_or_default();
        drops::note(Dropped::new(
            format!("tools[].type={kind}"),
            &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
            "dropping cross-protocol hosted (built-in) tool(s): a Responses hosted tool \
             has no function-tool equivalent for a non-Responses backend; forwarding it \
             would emit a malformed empty-name function tool the upstream rejects (400). \
             Route hosted-tool requests to a Responses lane to use them",
        ));
    }
    ir.tools.retain(|t| t.hosted.is_none());
    // Gemini `cachedContent` references a server-side context cache busbar cannot project into
    // `contents`. `extra` is wiped below (cross-protocol only), so the key vanishes with two
    // invisible consequences: the backend answers on the visible history only (cached turns absent)
    // and the caller is billed full uncached input. Failing closed would be terminal (both
    // `proxy/wire.rs` callers `Err => return`, pre-empting failover to a same-pool Gemini lane), so
    // degrade with a loud warn naming both consequences, as the two warns above do.
    // Gemini 3 thoughtSignature sentinel fill. A foreign client's tool-call history replayed against
    // a Gemini 3 AI-Studio backend has `ToolUse` blocks with `thought_signature: None`, but Gemini 3
    // requires one on every `functionCall` part or 400s ("missing a thought_signature").
    // `thought_signature_fill` is resolved by the caller (true only for a Gemini AI-Studio lane,
    // never Vertex; see the field's doc), so the gate stays here with `reasoning_allowed` /
    // `prompt_caching_allowed`. A real signature is never overwritten; only a `None` gets the sentinel.
    if prep.thought_signature_fill {
        let fill_blocks = |blocks: &mut [crate::codec::ir::IrBlock]| {
            for b in blocks {
                if let crate::codec::ir::IrBlock::ToolUse {
                    thought_signature, ..
                } = b
                {
                    if thought_signature.is_none() {
                        *thought_signature = Some(GEMINI_SKIP_THOUGHT_SIGNATURE.to_string());
                    }
                }
            }
        };
        for m in &mut ir.messages {
            fill_blocks(&mut m.content);
        }
    }
    drop_request_extra(ir, prep.ingress_protocol);
}

/// The members the caller's request holds in `extra` do not cross a translate attempt: each is
/// dropped through the one drop path, named by its wire path, and `extra` is emptied. Both hosts
/// (the exchange seam and the plane's own crossing, design F7) call this one function.
pub fn drop_request_extra(ir: &mut IrRequest, ingress_protocol: &str) {
    // OpenAI `messages[].name` (participant name), parked by the OpenAI Chat reader under
    // `MESSAGE_NAMES_SENTINEL`. No other protocol models it, so this is a target-protocol limit, but
    // it crossed silently and a multi-speaker transcript losing speaker labels changes what the model
    // is asked. The generic dropped-keys warn would name it only as an opaque `__busbar_…` key, so name
    // it in the caller's vocabulary, as `cachedContent` below. Keyed off the OpenAI module's own
    // const, not a copy: a rename that missed a duplicated literal would silently switch the warn off.
    if ir
        .extra
        .contains_key(crate::codec::dialect::MESSAGE_NAMES_SENTINEL)
    {
        drops::note(Dropped::new(
            crate::codec::dialect::MESSAGE_NAMES_PATH,
            &crate::codec::diagnostics::IR_DROP_MESSAGE_NAME,
            "dropping OpenAI `messages[].name` on the cross-protocol seam: no target \
             protocol models a per-message participant name, so a multi-speaker \
             transcript reaches the backend with its speaker labels removed. Put the \
             speaker in the message TEXT, or route to an openai lane",
        ));
    }
    if ir.extra.contains_key(CACHED_CONTENT) {
        drops::note(Dropped::new(
            CACHED_CONTENT,
            &crate::codec::diagnostics::IR_DROP_CACHED_CONTENT,
            "dropping Gemini `cachedContent` on the cross-protocol seam: the referenced \
             context cache lives server-side at Google and cannot be projected into \
             `contents`, so (1) the backend answers on the VISIBLE history only — the \
             cached turns are absent, not summarized — and (2) the caller is billed FULL \
             UNCACHED input for this request. Route cachedContent requests to a Gemini \
             lane.",
        ));
    }
    // Every remaining member `extra` holds dies here: the source dialect's members its map file does
    // not model, and those its reader parks, named as the reader declares them (a spelling hint or a
    // member its own code carries names nothing). Only paths are logged, never values (caller payload).
    let paths = super::proto_codec::with_reader(ingress_protocol, |r| {
        drops::extra_paths(&ir.extra, r.grammar().parked, |path| {
            crate::codec::carry::crosses(r.grammar().request_map, path)
        })
    })
    .unwrap_or_else(|| drops::extra_paths(&ir.extra, &[], |_| false));
    for path in paths {
        drops::note(Dropped::new(
            path,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            "dropping an unmapped request member on the cross-protocol seam: the source \
             dialect's member has no form in the target dialect, so it is NOT forwarded to the \
             backend. Route the request to a same-protocol lane (which forwards the caller's \
             bytes) if the member is load-bearing",
        ));
    }
    ir.extra.clear();
}

/// What a translate attempt's answer drops: the far end's vendor-scoped response metadata, content
/// blocks its reader does not model, and every other member its dialect does not carry (the answer
/// walk), each named by wire path. Both hosts call it inside a [`drops::scope`].
pub fn drop_untranslatable_response(egress_protocol: &str, body: &Value) {
    crate::codec::dialect::drop_untranslatable_response_metadata(egress_protocol, body);
    super::proto_codec::with_reader(egress_protocol, |r| {
        drops::note_unmodelled_blocks(
            r.grammar().response_blocks,
            body,
            drops::UNMODELLED_ANSWER_BLOCK,
        );
        // Every other answer member the caller's dialect does not get (DF-MAP-IR-GAPS section E: no
        // extra bucket), walked against what the far end's dialect carries.
        if let Some(carried) = r.grammar().response_carried {
            drops::note_unmapped(&carried, "", body);
        }
    });
}

/// Chat cross-protocol ingress preparation. Clears, as normalizations not drops:
/// - `id`: every dialect's id is a format (`chatcmpl-…`, `msg_…`, `resp_…`); the ingress writer mints
///   its own (the synthesized-ID contract on [`IrResponse::id`]). The stream path clears it on `MessageStart`.
/// - `system_fingerprint`: only the OpenAI Chat reader sets it and only its writer emits it, and a
///   cross-protocol response never has Chat on both ends.
///
/// Not cleared: `stop_sequence`, the matched stop string. The Anthropic writer emits it and Bedrock
/// carries it (`additionalModelResponseFields.stop_sequence`), so it maps. Clearing it nulled it for
/// a Bedrock to Anthropic buffered answer while the stream path kept it.
pub fn chat_prepare_for_ingress(ir: &mut IrResponse, ingress_protocol: &str, now_epoch: u64) {
    ir.id = None;
    ir.system_fingerprint = None;
    if ir.created.is_none() {
        ir.created = Some(now_epoch);
    }
    super::proto_codec::ToolIdRemap::default().remap_response(ingress_protocol, ir);
    envelope_foreign_signatures(ir, ingress_protocol);
}

/// IR-18: a reasoning signature another family minted is wrapped in the busbar provenance envelope
/// before the ingress writer places it in a client carrier with no origin field, so the next turn
/// brings back the original bytes and origin (the ingress reader unwraps it), not a blob its own
/// family would reject.
fn envelope_foreign_signatures(ir: &mut IrResponse, ingress_protocol: &str) {
    let Some(p) = super::proto_codec::protocol_for(ingress_protocol) else {
        return;
    };
    let writer = p.writer();
    for block in &mut ir.content {
        if let crate::codec::ir::IrBlock::Thinking {
            signature: Some(sig),
            signature_origin,
            ..
        } = block
        {
            *sig = crate::codec::ir::sig_envelope::for_client(sig, *signature_origin, |o| {
                writer.reads_signature_origin_as_own(o)
            });
        }
    }
}

/// Chat token-usage billing projection. It delegates to [`crate::codec::ir::IrUsage::to_token_usage`],
/// the function the streaming A-tap and same-protocol usage tap ledger through, so one completion
/// bills one figure whatever the caller's `stream` flag (a separate projection once could drift).
pub fn chat_usage(r: &IrResponse) -> Option<Billing> {
    Some(Billing::Tokens(r.usage.to_token_usage()))
}

/// The chat request handle: the request, plus the egress lane's declared capabilities, which the
/// seam hands over in the [`EgressPrep`] and the egress write passes to the writer.
pub struct ChatReqHandle(
    pub IrRequest,
    pub busbar_contract::ir::egress_prep::LaneCaps,
);
/// The chat response handle: the response, and the translation cap its plane enforces
/// ([`IrHandle::apply_translate_cap`]; [`busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT`]
/// until set), which bounds what a writer accumulates when re-emitting the response as a stream.
pub struct ChatRespHandle(pub IrResponse, pub usize);

impl Sealed for ChatReqHandle {}
impl Sealed for ChatRespHandle {}

impl IrHandle for ChatReqHandle {
    fn verb(&self) -> OpVerb {
        OpVerb::CHAT
    }
    fn wants_stream(&self) -> bool {
        self.0.stream
    }
    fn facts(&self) -> Box<dyn IrFacts + Send + Sync> {
        Box::new(self.0.clone())
    }
    fn prepare_for_egress(&mut self, prep: &EgressPrep) {
        chat_prepare_for_egress(&mut self.0, prep);
        self.1 = prep.lane_caps;
    }
    fn egress_dropped_controls(&self, egress_proto: &str) -> Vec<&'static str> {
        super::proto_codec::protocol_for(egress_proto)
            .map(|p| {
                p.writer()
                    .dropped_egress_controls_for_lane(&self.0, &self.1)
            })
            .unwrap_or_default()
    }
    fn write_egress_request(
        &mut self,
        host: &dyn busbar_contract::codec::CodecHost,
        egress_proto: &str,
        model: &str,
    ) -> EgressWire {
        // Chat is always a JSON body the router post-shapes. The lane model rides along for dialects
        // whose spelling depends on the model family, and the lane's capabilities (recorded at
        // `prepare_for_egress`) for dialects whose spelling depends on what it accepts.
        super::proto_codec::protocol_for(egress_proto)
            .map(|p| {
                let writer = p.writer();
                if !writer.carries_image_detail() {
                    warn_dropped_image_details(&self.0);
                }
                EgressWire::Json(writer.write_request_for_lane(host, &self.0, model, &self.1))
            })
            .unwrap_or_else(|| EgressWire::Bytes(SlabBytes::default()))
    }
}

impl IrHandle for ChatRespHandle {
    fn verb(&self) -> OpVerb {
        OpVerb::CHAT
    }
    fn billing(&self) -> Option<Billing> {
        chat_usage(&self.0)
    }
    // `fill_response_model_if_absent` is deliberately not overridden (the default is a no-op): a
    // cross-protocol response with no upstream model reaches the ingress writer with `model = None`
    // and each writer renders its own native shape (anthropic `""`, openai/responses a default name,
    // cohere/gemini omit the key). Stamping the lane's config name changed the body against the wire.
    fn prepare_for_ingress(&mut self, ingress_protocol: &str, now_epoch: u64) {
        chat_prepare_for_ingress(&mut self.0, ingress_protocol, now_epoch);
    }
    // Stash the original ingress request body verbatim. Which members a writer echoes is its own
    // business (`ResponsesWriter::write_response` reads specific keys; others ignore it), so no
    // `match ingress_protocol` here.
    fn apply_request_echo(&mut self, ingress_request_body: &Value) {
        self.0.request_echo = Some(ingress_request_body.clone());
    }
    fn apply_translate_cap(&mut self, cap: usize) {
        self.1 = cap;
    }
    fn wrap_buffered_as_stream(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ingress_protocol: &str,
        elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        // A client that asked to stream must get its dialect's stream even when the upstream answered
        // with one buffered body. The writer gets first say (Bedrock synthesizes binary eventstream);
        // SSE dialects use the shared synthesizer, which drives the response through the live translator.
        super::proto_codec::protocol_for(ingress_protocol)
            .and_then(|p| {
                p.writer()
                    .wrap_buffered_as_stream(host, &self.0, elapsed_ms)
            })
            .or_else(|| {
                super::proto_stream::StreamTranslate::synthesize_from_response(
                    host,
                    ingress_protocol,
                    &self.0,
                    self.1,
                )
            })
    }
    fn write_ingress_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ingress_protocol: &str,
        ingress_serves_op: bool,
    ) -> TranslatedResponse {
        if !ingress_serves_op {
            return TranslatedResponse::IngressUnsupported;
        }
        // Chat `write_response_value == Some(Value)` is Json.
        match super::proto_codec::protocol_for(ingress_protocol) {
            Some(p) => TranslatedResponse::Json(p.writer().write_response(host, &self.0)),
            None => TranslatedResponse::IngressUnsupported,
        }
    }
}

/// A protocol's chat operation. The field names the protocol whose `proto_codec` reader/writer are
/// this instance's codec; `read_request`/`read_response` yield `Box<dyn IrHandle>` (a
/// `ChatReqHandle`/`ChatRespHandle`) and the write half runs on those handles by egress-protocol
/// string. One type, six per-protocol instances registered by each dialect's `operation_handler(Operation::CHAT)`.
pub struct ChatOperation(pub &'static str);

impl ChatOperation {
    fn proto(&self) -> Option<super::proto_codec::Protocol> {
        super::proto_codec::protocol_for(self.0)
    }
}

impl OperationHandler for ChatOperation {
    fn extract_error(
        &self,
        status: u16,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        super::proto_codec::protocol_error(self.0, status, body)
    }

    fn streaming(&self) -> bool {
        true
    }
    fn taps_usage(&self) -> bool {
        true
    }
    fn wants_stream(&self, body: &Value) -> bool {
        body.get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    }
    fn body_affinity_key<'a>(&self, body: &'a Value) -> Option<&'a str> {
        body.get("system")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }
    fn extract_usage(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ingress_protocol: &str,
        body: &[u8],
    ) -> Result<Option<busbar_contract::billing::TokenUsage>, busbar_contract::codec::UsageTapFault>
    {
        // Same-protocol usage tap. Each way the tap cannot read the body is returned as its fault: the
        // door carries it on its answer and the kernel counts it and warns once per (protocol, reason);
        // the codec's own line is per-request detail, at debug.
        use busbar_contract::codec::UsageTapFault;
        let Some(p) = super::proto_codec::protocol_for(ingress_protocol) else {
            ::tracing::debug!(diag = %crate::codec::diagnostics::USAGE_TAP_UNKNOWN_PROTOCOL.banner(),
                protocol = ingress_protocol,
                "usage tap: unknown ingress protocol for a same-protocol 2xx body; \
                 billing 0 tokens for this request"
            );
            return Err(UsageTapFault::UnknownProtocol);
        };
        let v = match crate::codec::json::parse::<Value>(body) {
            Ok(v) => v,
            Err(_e) => {
                ::tracing::debug!(diag = %crate::codec::diagnostics::USAGE_TAP_BAD_JSON.banner(),
                    protocol = ingress_protocol,
                    error = %crate::codec::json::parse_err_log(body.len()),
                    "usage tap: failed to parse a same-protocol 2xx body as JSON; \
                     billing 0 tokens for this request"
                );
                return Err(UsageTapFault::BadJson);
            }
        };
        match p.reader().read_response(host, &v) {
            Ok(ir) => Ok(Some(ir.usage.to_token_usage())),
            Err(e) => {
                ::tracing::debug!(diag = %busbar_contract::diagnostic::USAGE_TAP_DECODE_FAILED.banner(),
                    protocol = ingress_protocol,
                    error = ?e,
                    "usage tap: read_response failed to decode a same-protocol 2xx body; \
                     billing 0 tokens for this request"
                );
                Err(UsageTapFault::Decode)
            }
        }
    }

    fn read_request_value(&self, v: &Value) -> Result<Box<dyn IrHandle>, IngressReject> {
        let p = self
            .proto()
            .ok_or_else(|| IngressReject::BadRequest(format!("unknown protocol {}", self.0)))?;
        p.reader()
            .read_request(v)
            .map(|r| Box::new(ChatReqHandle(r, Default::default())) as Box<dyn IrHandle>)
            .map_err(|e| IngressReject::BadRequest(format!("{e:?}")))
    }
    fn read_response_value(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        v: &Value,
    ) -> Result<Box<dyn IrHandle>, CodecError> {
        let p = self
            .proto()
            .ok_or_else(|| CodecError::Malformed(format!("unknown protocol {}", self.0)))?;
        p.reader()
            .read_response(host, v)
            .map(|r| {
                Box::new(ChatRespHandle(
                    r,
                    busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
                )) as Box<dyn IrHandle>
            })
            .map_err(|e| CodecError::Malformed(format!("{e:?}")))
    }
    fn read_request(
        &self,
        body: &[u8],
        _content_type: &str,
    ) -> Result<Box<dyn IrHandle>, IngressReject> {
        let v: Value =
            serde_json::from_slice(body).map_err(|e| IngressReject::BadRequest(e.to_string()))?;
        let p = self
            .proto()
            .ok_or_else(|| IngressReject::BadRequest(format!("unknown protocol {}", self.0)))?;
        p.reader()
            .read_request(&v)
            .map(|r| Box::new(ChatReqHandle(r, Default::default())) as Box<dyn IrHandle>)
            .map_err(|e| IngressReject::BadRequest(format!("{e:?}")))
    }
    fn read_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        wire: &[u8],
    ) -> Result<Box<dyn IrHandle>, CodecError> {
        let v: Value =
            serde_json::from_slice(wire).map_err(|e| CodecError::Malformed(e.to_string()))?;
        let p = self
            .proto()
            .ok_or_else(|| CodecError::Malformed(format!("unknown protocol {}", self.0)))?;
        p.reader()
            .read_response(host, &v)
            .map(|r| {
                Box::new(ChatRespHandle(
                    r,
                    busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT,
                )) as Box<dyn IrHandle>
            })
            .map_err(|e| CodecError::Malformed(format!("{e:?}")))
    }
}
