use super::*;
use crate::codec::keys;

/// Converse `usage.totalTokens`: every token consumed, cache tokens included. AWS reports
/// `inputTokens` as non-cached input only (full input is `inputTokens + cacheReadInputTokens +
/// cacheWriteInputTokens`; its worked example totals 16 + 4 + 695 as 715). Since `write_cache_usage`
/// emits the cache counts as additive siblings, omitting them would publish parts summing past the
/// stated total and drop tokens from an accounting consumer's books. The reader never reads
/// `totalTokens`, so this is the only place the wire total is decided (buffered body and streamed
/// `metadata` frame).
///
/// Adds are `saturating_add`: the operands are upstream-controlled, and a bare `+` near `u64::MAX`
/// would panic under overflow-checks or wrap in release. Mirrors the Gemini writer.
fn converse_total_tokens(usage: &crate::codec::ir::IrUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
}

impl ProtocolWriter for BedrockWriter {
    /// No image-fidelity slot: an image's `detail` is dropped, with the seam's warn.
    fn carries_image_detail(&self) -> bool {
        false
    }

    fn upstream_path(&self) -> &str {
        "/model"
    }

    fn upstream_path_for(&self, model: &str) -> String {
        format!("/model/{}/converse", model)
    }

    fn upstream_path_for_stream(&self, model: &str, stream: bool) -> String {
        // streaming uses ConverseStream (binary application/vnd.amazon.eventstream response).
        if stream {
            format!("/model/{}/converse-stream", model)
        } else {
            format!("/model/{}/converse", model)
        }
    }

    // The model is in the URL, not the body, so this is a no-op reporting `false` (a same-protocol
    // passthrough is never made non-pristine by model rewriting).
    fn rewrite_model_if_needed(&self, _body: &mut serde_json::Value, _model: &str) -> bool {
        false
    }

    // `inferenceConfig.maxTokens` is optional on Converse (the model default applies and an empty
    // `inferenceConfig` is omitted), so `requires_max_tokens` is not overridden: a default would cap output.

    /// Converse frames a turn as `{role, content: [{text}]}`; a verbatim insert of `{role, content:
    /// "…"}` messages would corrupt it (it also spells its container `messages`), so this override
    /// is load-bearing.
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> bool {
        if !obj
            .get(keys::MESSAGES)
            .is_some_and(serde_json::Value::is_array)
        {
            return false;
        }
        let Some(pairs) = crate::codec::dialect::rewrite_text_pairs(messages) else {
            return false;
        };
        let framed: Vec<serde_json::Value> = pairs
            .into_iter()
            .map(|(role, text)| serde_json::json!({ (keys::ROLE): role, (keys::CONTENT): [{ (keys::TEXT): text }] }))
            .collect();
        obj.insert(keys::MESSAGES.to_string(), serde_json::Value::Array(framed));
        true
    }

    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        self.dropped_egress_controls_for_lane(req, &LaneCaps::default())
    }

    fn dropped_egress_controls_for_lane(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> Vec<&'static str> {
        // Mirrors the `write_request` warns: every control this writer drops for this lane.
        let mut dropped = Vec::new();
        // BED-08: a JSON-schema directive projects onto `outputConfig.textFormat` on a lane declaring
        // native structured outputs; any other directive, and every directive elsewhere, is dropped.
        if req.response_format.as_ref().is_some_and(|rf| {
            !caps.native_structured_output || super::write_bedrock_text_format(rf).is_none()
        }) {
            dropped.push(keys::RESPONSE_FORMAT);
        }
        if matches!(req.tool_choice, Some(crate::codec::ir::IrToolChoice::None)) {
            dropped.push("tool_choice=none");
        }
        dropped.extend(
            crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
                .map(crate::codec::carry::Slot::name),
        );
        dropped.extend(req.hosted_tools.iter().map(|h| h.kind_str()));
        if req.reasoning.is_some() {
            dropped.push(crate::codec::drops::name::REASONING);
        }
        dropped
    }

    /// The production write: the lane's [`LaneCaps`] choose the structured-output form (see
    /// `write_request_with`). A cross-protocol reasoning ask is dropped for every model family.
    fn write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        self.write_request_with(req, caps)
    }

    fn write_response_event(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match ev {
            IrStreamEvent::MessageStart { .. } => Some((
                ET_MESSAGE_START.to_string(),
                serde_json::json!({ (keys::ROLE): keys::ASSISTANT }),
            )),

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => match block {
                // A text block has no `contentBlockStart`: the `ContentBlockStart$start` union has
                // `toolUse` (plus `image`/`toolResult` in newer revisions), no text member. A real
                // stream opens text implicitly on the first text `contentBlockDelta` and closes it with
                // `contentBlockStop`, as the reader mirrors. An empty `start: {}` is off-spec, a proxy
                // tell. `mark_block_open` still fires so `BlockStop` emits the stop, but no start frame.
                crate::codec::ir::IrBlockMeta::Text => {
                    self.mark_block_open(*index);
                    None
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    self.mark_block_open(*index);
                    Some((
                        ET_CONTENT_BLOCK_START.to_string(),
                        serde_json::json!({
                            (super::CONTENT_BLOCK_INDEX): index,
                            (keys::START): { (super::TOOL_USE_CAMEL): { (super::TOOL_USE_ID): id, (keys::NAME): name } }
                        }),
                    ))
                }
                // A reasoning block has no `contentBlockStart` either (the union models only `toolUse`).
                // Plaintext and redacted reasoning both stream via
                // `contentBlockDelta.delta.reasoningContent` (`text` / `signature` / `redactedContent`)
                // and close with `contentBlockStop`; an emitted `start.reasoningContent` is off-spec.
                // `mark_block_open` still fires, no start frame; redacted bytes ride the following
                // `RedactedReasoningDelta`. Image has no start projection and is not marked open, so it
                // emits no orphan stop.
                crate::codec::ir::IrBlockMeta::Thinking { .. }
                | crate::codec::ir::IrBlockMeta::RedactedThinking => {
                    self.mark_block_open(*index);
                    None
                }
                crate::codec::ir::IrBlockMeta::Image => None,
            },

            IrStreamEvent::BlockDelta { index, delta } => match delta {
                crate::codec::ir::IrDelta::TextDelta(text) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (keys::TEXT): text }
                    }),
                )),

                crate::codec::ir::IrDelta::InputJsonDelta(json_str) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::TOOL_USE_CAMEL): { (keys::INPUT): json_str } }
                    }),
                )),

                // Streamed thinking. Each `ReasoningContentBlockDelta` frame carries one of `text`,
                // `signature` or `redactedContent`, so each IR delta is exactly one frame. Streaming
                // inverse of `bedrock_reasoning_block`.
                crate::codec::ir::IrDelta::ThinkingDelta(text) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (keys::TEXT): text } }
                    }),
                )),

                // A reasoning signature token re-emits under `signature`.
                crate::codec::ir::IrDelta::SignatureDelta(sig) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (keys::SIGNATURE): sig } }
                    }),
                )),
                // A redacted-reasoning delta re-emits its bytes under `redactedContent`, never as a
                // plaintext `signature`. IR-21: the stream has no generated-media delta member.
                crate::codec::ir::IrDelta::MediaDelta(_) => None,
                crate::codec::ir::IrDelta::RedactedReasoningDelta(redacted) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (super::REDACTED_CONTENT): redacted } }
                    }),
                )),
                // Streamed citations: `ContentBlockDelta` has a `citation` member (same field set as the
                // buffered `Citation`) interleaved with `text` at the same `contentBlockIndex`.
                // Suppressing it made `stream: false` return sources and `stream: true` none.
                //
                // A native `citation` delta carries one citation, so this single-frame arm frames the
                // first of the batch; `write_response_events` walks a multi-citation delta, re-entering
                // here per citation. A direct caller therefore still gets a well-formed frame.
                crate::codec::ir::IrDelta::CitationsDelta(cits) => {
                    let c = cits.first()?;
                    match super::write_bedrock_citation(c) {
                        Some(citation) => Some((
                            ET_CONTENT_BLOCK_DELTA.to_string(),
                            serde_json::json!({
                                (super::CONTENT_BLOCK_INDEX): index,
                                (keys::DELTA): { (keys::CITATION): citation }
                            }),
                        )),
                        // All neutral fields empty: nothing for the union, and `{}` would be malformed. Dropped with a warn.
                        None => {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::TEXT,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [],
                                "dropping a streamed citation on a bedrock egress: it carried no \
                                 title, url, quoted text or resolvable character location, so there \
                                 is no populated member for the Converse `citation` delta union");
                            None
                        }
                    }
                }
                // Converse has no logprobs shape; dropped.
                crate::codec::ir::IrDelta::LogprobsDelta(_) => None,
            },

            // An untracked index (Image, whose start had no projection) is not closed, which would
            // orphan a `contentBlockStop`.
            IrStreamEvent::BlockStop { index } => {
                if self.take_block_open(*index) {
                    Some((
                        ET_CONTENT_BLOCK_STOP.to_string(),
                        serde_json::json!({ (super::CONTENT_BLOCK_INDEX): index }),
                    ))
                } else {
                    None
                }
            }

            // The wire has `stopReason` in `messageStop` and `usage` in a separate following `metadata`
            // frame, but the IR has one combined `MessageDelta{stop_reason, usage}`. One return cannot
            // emit two frames, so the fan-out lives in `StreamTranslate::translate_event` (proto/mod.rs),
            // which splits it into a stop-only delta and a usage-only delta and injects the real
            // `metrics.latencyMs` onto `metadata`. This arm maps each already-split delta:
            //   - stop_reason = Some  to `messageStop` (usage ignored)
            //   - stop_reason = None  to `metadata` with the token usage (no `metrics`: the fan-out adds
            //                         the real elapsed time, and a fabricated `latencyMs: 0` was a tell)
            // The stream has no stop_sequence field. IR-16 (BED-10): a context-window stop detail is
            // written as `model_context_window_exceeded`.
            IrStreamEvent::MessageDelta {
                stop_reason,
                usage,
                stop_sequence: _,
                stop_detail,
            } => match stop_reason {
                Some(reason) => Some((
                    ET_MESSAGE_STOP.to_string(),
                    serde_json::json!({
                        (super::STOP_REASON): stop_reason_reverse_detailed(*reason, stop_detail.as_ref())
                    }),
                )),
                None => {
                    let mut usage_obj = serde_json::Map::new();
                    usage_obj.insert(
                        super::INPUT_TOKENS_CAMEL.to_string(),
                        usage.input_tokens.into(),
                    );
                    usage_obj.insert(
                        super::OUTPUT_TOKENS_CAMEL.to_string(),
                        usage.output_tokens.into(),
                    );
                    // Cache-inclusive and saturating; see `converse_total_tokens`.
                    usage_obj.insert(
                        super::TOTAL_TOKENS_CAMEL.to_string(),
                        converse_total_tokens(usage).into(),
                    );
                    write_cache_usage(&mut usage_obj, usage);
                    let mut metadata = serde_json::json!({ (keys::USAGE): usage_obj });
                    if let (Some(tier), Some(obj)) =
                        (super::served_tier_member(usage), metadata.as_object_mut())
                    {
                        obj.insert(super::SERVICE_TIER_CAMEL.to_string(), tier);
                    }
                    Some((ET_METADATA.to_string(), metadata))
                }
            },

            IrStreamEvent::MessageStop => None,

            // A mid-stream error. The native form is a modeled-exception event-stream frame
            // (`:message-type: exception`), which `StreamTranslate` emits via `write_response_exception`
            // before reaching here. This arm fires only for a non-eventstream consumer, and falls back
            // to an `event`-typed frame naming a real stream-union exception
            // (`bedrock_stream_exception_for`, not the request-level HTTP set), never a literal `"error"`.
            IrStreamEvent::Error(err) => {
                let (exception_name, message) = bedrock_stream_exception_for(err);
                Some((
                    exception_name.to_string(),
                    serde_json::json!({ (keys::MESSAGE): message }),
                ))
            }
        }
    }

    /// A `citation` delta carries one citation, so a multi-citation delta frames as several events at
    /// the same `contentBlockIndex`, as a native stream interleaves them. The framing seam splits
    /// them (`max_citations_per_delta`), but the plane codec calls this directly, so a whole batch
    /// must emit every citation. Each re-enters the single-frame arm with a one-citation delta, the
    /// one source of truth for the delta's shape. Other events keep the base one-frame behaviour.
    fn write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        if let IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::CitationsDelta(cits),
        } = ev
        {
            if cits.len() > 1 {
                return cits
                    .iter()
                    .filter_map(|c| {
                        self.write_response_event(
                            host,
                            &IrStreamEvent::BlockDelta {
                                index: *index,
                                delta: crate::codec::ir::IrDelta::CitationsDelta(vec![c.clone()]),
                            },
                        )
                    })
                    .collect();
            }
        }
        self.write_response_event(host, ev).into_iter().collect()
    }

    /// Maps an IR error to the `(exception_name, message)` of the modeled-exception event-stream
    /// frame (`:message-type: exception`) `StreamTranslate` emits via
    /// `eventstream::encode_exception_frame`, using `bedrock_stream_exception_for` (the five-member
    /// stream union, not the request-level HTTP set) so an SDK decoder recognizes it. Shared with
    /// the `write_response_event` Error arm.
    fn write_response_exception(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        err: &busbar_contract::protocol::IrError,
    ) -> Option<(String, String)> {
        let (exception_name, message) = bedrock_stream_exception_for(err);
        Some((exception_name.to_string(), message))
    }

    fn write_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        let mut content_arr: Vec<serde_json::Value> = Vec::new();

        for block in &resp.content {
            match block {
                crate::codec::ir::IrBlock::Text {
                    text, citations, ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    // Citations in Converse's native `citationsContent` slot, the buffered twin of the
                    // streaming `citation` frame (else sources appeared only when streaming).
                    // `citationsContent` replaces the plain `text` member (the text is inside it), so it
                    // is used only when at least one citation projects; otherwise the plain `{"text": …}` shape stays.
                    let cits: Vec<serde_json::Value> = citations
                        .iter()
                        .filter_map(super::write_bedrock_citation)
                        .collect();
                    if cits.is_empty() {
                        if !citations.is_empty() {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::TEXT,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [dropped = citations.len(), ],
                                "dropping citation(s) on a bedrock response egress: none carried a \
                                 title, url, quoted text or resolvable character location, so there \
                                 is no populated member for the Converse `Citation` shape");
                        }
                        content_arr.push(serde_json::json!({ (keys::TEXT): text }));
                    } else {
                        content_arr.push(serde_json::json!({
                            (super::CITATIONS_CONTENT): {
                                (keys::CONTENT): [{ (keys::TEXT): text }],
                                (keys::CITATIONS): cits
                            }
                        }));
                    }
                }

                // A model does not emit an attachment on the Converse response surface.
                crate::codec::ir::IrBlock::Media { .. } => {}

                crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } => {
                    content_arr.push(serde_json::json!({
                        (super::TOOL_USE_CAMEL): {
                            (super::TOOL_USE_ID): id,
                            (keys::NAME): name,
                            (keys::INPUT): input
                        }
                    }));
                }

                crate::codec::ir::IrBlock::Image { source, .. } => {
                    // An assistant response can carry an Image block, a native `{"image": ...}` block.
                    // A source with no Bedrock projection (URL / file_id) is omitted with a trace by the helper.
                    if let Some(image_block) = bedrock_image_block(source) {
                        content_arr.push(serde_json::json!({ (keys::IMAGE): image_block }));
                    }
                }
                crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                    // Structured JSON is only a tool-result member; omitted from an assistant turn.
                }

                crate::codec::ir::IrBlock::Thinking {
                    text,
                    signature,
                    redacted,
                    ..
                } => {
                    // Re-emit the model's reasoning as a native `reasoningContent` block (inverse of
                    // `read_response`): `redactedContent` for a redacted block, else `reasoningText`.
                    content_arr.push(bedrock_reasoning_block(text, signature, *redacted));
                }

                // A `toolResult` is a user-turn block with no place in an assistant response: the only
                // genuine no-op here, listed explicitly.
                crate::codec::ir::IrBlock::ToolResult { .. } => {}
            }
        }

        // Converse rejects an assistant message with empty `content` (ValidationException), as
        // `write_request` guards every turn. A response whose blocks were all non-representable would
        // emit `content: []`, so insert a minimal placeholder text block.
        if content_arr.is_empty() {
            content_arr.push(serde_json::json!({ (keys::TEXT): "" }));
        }

        // IR-16 (BED-10): a context-window detail is `model_context_window_exceeded`.
        let reverse_reason = stop_reason_reverse_detailed(
            resp.stop_reason
                .unwrap_or(crate::codec::ir::IrStopReason::EndTurn),
            resp.stop_detail.as_ref(),
        );

        // No body identity: the Converse body has no id or `created` (the request id is only the
        // `x-amzn-RequestId` header), so a synthesized one would be a proxy tell. Cross-protocol
        // ingress writers that need an id synthesize it themselves. `stopReason` and `usage` are
        // reproduced exactly from the IR, so a same-protocol round-trip is byte-identical.
        let mut usage_obj = serde_json::Map::new();
        usage_obj.insert(
            super::INPUT_TOKENS_CAMEL.to_string(),
            resp.usage.input_tokens.into(),
        );
        usage_obj.insert(
            super::OUTPUT_TOKENS_CAMEL.to_string(),
            resp.usage.output_tokens.into(),
        );
        // Cache-inclusive and saturating, as in the streaming `metadata` frame (see
        // `converse_total_tokens`); keeps a same-protocol round-trip byte-identical with cache tokens.
        usage_obj.insert(
            super::TOTAL_TOKENS_CAMEL.to_string(),
            converse_total_tokens(&resp.usage).into(),
        );
        write_cache_usage(&mut usage_obj, &resp.usage);

        let mut answer = serde_json::json!({
            (keys::OUTPUT): {
                (keys::MESSAGE): {
                    (keys::ROLE): keys::ASSISTANT,
                    (keys::CONTENT): content_arr
                }
            },
            (super::STOP_REASON): reverse_reason,
            (keys::USAGE): usage_obj
        });
        if let (Some(tier), Some(obj)) = (
            super::served_tier_member(&resp.usage),
            answer.as_object_mut(),
        ) {
            obj.insert(super::SERVICE_TIER_CAMEL.to_string(), tier);
        }
        answer
    }

    /// Native Converse error envelope: a flat body with the detail in lowercase `message` and the
    /// exception name in `__type` (the two fields `BedrockReader::extract_error` reads). The generic
    /// `{"error":{...}}` envelope would fail to decode in an AWS SDK. `kind` maps through
    /// `error_kind_to_bedrock_type` so `__type` is a real Converse exception name. `application/json`.
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            (super::DUNDER_TYPE): error_kind_to_bedrock_type(kind),
            (keys::MESSAGE): message,
        })
    }

    fn attach_error_response_headers(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        headers: &mut busbar_contract::http::HeaderMap,
        kind: &str,
        _envelope: &serde_json::Value,
    ) {
        // A real response always carries `x-amzn-RequestId` and `x-amzn-errortype` (== body
        // `__type`); omitting them was distinguishable and left the SDK request id empty.
        attach_bedrock_error_headers(host, headers, kind);
    }

    fn new_stream_framing(&self) -> Box<dyn super::StreamFraming> {
        // Per-stream framing: the messageStop/metadata two-frame deferral and the one-metadata
        // invariant, kept here so the translator names no Bedrock wire shape.
        Box::<BedrockStreamFraming>::default()
    }

    fn wrap_buffered_as_stream(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ir: &crate::codec::ir::IrResponse,
        elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        // A client that asked for ConverseStream but got a buffered 2xx needs a native eventstream
        // frame sequence; a bare JSON body fails the SDK decoder. Synthesized through this same writer.
        Some(bedrock_response_to_eventstream(host, ir, elapsed_ms))
    }

    fn inject_response_metrics(&self, value: &mut serde_json::Value, elapsed_ms: Option<u64>) {
        // A native response always populates `metrics.latencyMs` (required by the service model). The
        // writer's `write_response` omits it (timing unknown there), so inject the elapsed time here.
        // An existing well-formed `metrics` is kept, and it is emitted even without timing, as in the
        // streaming `metadata` frame.
        super::ensure_metrics(value, elapsed_ms);
    }

    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        // Forward the captured upstream `x-amzn-RequestId` same-protocol; otherwise synthesize
        // (non-stream/cross-protocol passes `None`). Synthesis failure omits the header.
        upstream_request_id
            .map(String::from)
            .or_else(|| synth_amzn_request_id(host))
            .map(|id| (HDR_AMZN_REQUEST_ID, id))
    }

    /// IR-18: a `reasoningText.signature` reads back as Claude's or the Bedrock model's own (told
    /// apart by the conversation's model id).
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        matches!(
            origin,
            crate::codec::ir::IrSignatureOrigin::Anthropic
                | crate::codec::ir::IrSignatureOrigin::BedrockOther
        )
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }
}

impl BedrockWriter {
    /// The request write for a lane with the declared [`LaneCaps`] (the model gate has already
    /// run — see `write_request_for_lane`). `write_request` is this with the default capabilities.
    fn write_request_with(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();

        // The captured native `cachePoint` markers (see `CACHE_POINTS_SENTINEL`). On a same-protocol
        // passthrough this carries the prompt-cache markers the reader stashed; on cross-protocol
        // egress `extra` is cleared so this is absent and no Bedrock-only marker leaks onto a foreign
        // wire. Borrowed once here; the `system`/`messages` sub-arrays are spliced back below and the
        // sentinel is then SKIPPED by the trailing extra-merge so it never reaches the wire.
        let cache_points = req
            .extra
            .get(CACHE_POINTS_SENTINEL)
            .and_then(|v| v.as_object());
        let system_cache_points = cache_points
            .and_then(|cp| cp.get(keys::SYSTEM))
            .and_then(|v| v.as_array());
        let message_cache_points = cache_points
            .and_then(|cp| cp.get(keys::MESSAGES))
            .and_then(|v| v.as_array());

        // The captured native `guardContent` markers (see `GUARD_CONTENT_SENTINEL`); same stash
        // shape as the cachePoint markers and spliced back via the same shared helper. Consumed here
        // and SKIPPED by the trailing extra-merge so the sentinel never reaches the wire.
        let guard_content = req
            .extra
            .get(GUARD_CONTENT_SENTINEL)
            .and_then(|v| v.as_object());
        let system_guard_content = guard_content
            .and_then(|gc| gc.get(keys::SYSTEM))
            .and_then(|v| v.as_array());
        let message_guard_content = guard_content
            .and_then(|gc| gc.get(keys::MESSAGES))
            .and_then(|v| v.as_array());

        // Captured top-level `document` / `video` markers (`DOC_VIDEO_SENTINEL`), spliced back via
        // the same helper as guardContent. Consumed here and skipped by the trailing extra-merge so
        // the sentinel never reaches the wire. Only messages carry them.
        let doc_video = req
            .extra
            .get(DOC_VIDEO_SENTINEL)
            .and_then(|v| v.as_object());
        let message_doc_video = doc_video
            .and_then(|dv| dv.get(keys::MESSAGES))
            .and_then(|v| v.as_array());

        // When the positional cachePoint stash is present (same-protocol passthrough) it owns
        // placement (spliced below for byte-identity), so inline `cache_control` emission is
        // suppressed to avoid a double marker. Cross-protocol, `extra` is cleared and the inline
        // emission from IR `cache_control` is the sole carrier (an Anthropic breakpoint to a cachePoint).
        let emit_inline_system_cache = system_cache_points.is_none();
        if !req.system.is_empty() || system_cache_points.is_some() || system_guard_content.is_some()
        {
            let mut text_arr: Vec<serde_json::Value> = Vec::new();
            // System blocks the reader modelled out of a stashed `guardContent` (BED-02); the stash
            // splice re-emits the verbatim block, so the modelled copy is not written.
            let guard_modelled = stashed_ir_indices([system_guard_content], None);
            for (sys_idx, block) in req.system.iter().enumerate() {
                if guard_modelled.contains(&sys_idx) {
                    continue;
                }
                if let crate::codec::ir::IrBlock::Text {
                    text,
                    cache_control,
                    ..
                } = block
                {
                    text_arr.push(serde_json::json!({ (keys::TEXT): text }));
                    // Emit a `cachePoint` after the block carrying the `cache_control` boundary (the
                    // breakpoint closes the prefix before it). Suppressed when the stash owns placement.
                    if emit_inline_system_cache && cache_control.is_some() {
                        text_arr.push(bedrock_cache_point());
                    }
                }
            }

            // Re-emit captured `cachePoint` / `guardContent` markers at their original positions.
            // Both recorded indices against the same original array, so they are spliced as one
            // sorted batch (`merge_marker_entries`); two passes would shift the second's indices.
            let merged = merge_marker_entries(system_cache_points, system_guard_content);
            splice_cache_points(&mut text_arr, &merged);

            if !text_arr.is_empty() {
                out.insert(keys::SYSTEM.to_string(), serde_json::Value::Array(text_arr));
            }
        }

        // Same gate as the system array: the message-cachePoint stash owns placement when present;
        // cross-protocol (stash cleared) inline emission is the sole carrier.
        let emit_inline_message_cache = message_cache_points.is_none();
        let mut msgs_arr: Vec<serde_json::Value> = Vec::new();
        for (msg_idx, msg) in req.messages.iter().enumerate() {
            let role_str = match msg.role {
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                // A Tool-role message carries `toolResult` blocks; Converse has no "tool" role, a tool
                // result is a block in a user-turn message, so Tool maps to "user".
                crate::codec::ir::IrRole::Tool => keys::USER,
                // System text is extracted into `req.system`, so a System-role message should not
                // reach the wire. If one escapes, skip it rather than mislabel it as a user turn.
                // Each role is explicit; no catch-all.
                crate::codec::ir::IrRole::System => continue,
            };

            let mut content_arr: Vec<serde_json::Value> = Vec::new();
            // IR blocks the reader modelled out of a wire block it also parked verbatim
            // (`document` / `video` / `guardContent`), keyed by the reader's IR index `b`.
            let stashed_here =
                stashed_ir_indices([message_doc_video, message_guard_content], Some(msg_idx));
            for (block_idx, block) in msg.content.iter().enumerate() {
                // A block the reader also parked verbatim (`DOC_VIDEO_SENTINEL` / `GUARD_CONTENT_SENTINEL`)
                // at this (message, block) position. The stash is spliced back below, so writing the
                // modelled projection too would emit it twice. The verbatim block wins, keeping a
                // Bedrock-to-Bedrock round-trip byte-identical (`citations`, `context`, guard
                // `qualifiers` included); a cross-protocol IR has no stash and takes the modelled path.
                // Matched on `b` (the IR index), not `i` (the wire slot): they differ by the wire-only
                // blocks (`cachePoint`) before it, and comparing `i` emitted the attachment twice.
                if stashed_here.contains(&block_idx) {
                    continue;
                }
                // The cache boundary on this block, emitted as a `cachePoint` immediately after it
                // (where Bedrock expects it), suppressed when the stash owns placement. BED-07:
                // Thinking / Image / Media carry one too, so they project a `cachePoint` like Text.
                let block_cache_control = match block {
                    crate::codec::ir::IrBlock::Text { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolUse { cache_control, .. }
                    | crate::codec::ir::IrBlock::ToolResult { cache_control, .. }
                    | crate::codec::ir::IrBlock::Thinking { cache_control, .. }
                    | crate::codec::ir::IrBlock::Image { cache_control, .. }
                    | crate::codec::ir::IrBlock::Media { cache_control, .. } => {
                        cache_control.as_ref()
                    }
                    crate::codec::ir::IrBlock::Json(_)
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. } => None,
                };
                // The projection may be nothing (an image with no Converse source, an audio
                // attachment); a cachePoint follows only a block actually written.
                let written_before = content_arr.len();
                match block {
                    // COH-17: an empty text block carrying only citations is omitted (Converse
                    // rejects blank text). The search-result slot (a caller's RAG passage, from an
                    // Anthropic `search_result` or a Converse `searchResult` with no parked raw block)
                    // re-emits as the native `searchResult` block.
                    b @ crate::codec::ir::IrBlock::Text { .. }
                        if b.as_search_result().is_some() =>
                    {
                        if let Some(sr) = b.as_search_result() {
                            content_arr.push(super::write_bedrock_search_result(&sr));
                        }
                    }
                    b @ crate::codec::ir::IrBlock::Text { .. } if b.is_citation_carrier() => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::TEXT,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping citations with no text on Bedrock egress: blank text is \
                             rejected (COH-17)"
                        );
                    }
                    crate::codec::ir::IrBlock::Text {
                        text, citations, ..
                    } => {
                        // BED-13: a cited assistant-history text block is `citationsContent`, the text
                        // inside it beside its citations, as the response writer does. Uncited stays `{text}`.
                        let cits: Vec<serde_json::Value> = citations
                            .iter()
                            .filter_map(super::write_bedrock_citation)
                            .collect();
                        if cits.is_empty() {
                            content_arr.push(serde_json::json!({ (keys::TEXT): text }));
                        } else {
                            content_arr.push(serde_json::json!({
                                (super::CITATIONS_CONTENT): {
                                    (keys::CONTENT): [{ (keys::TEXT): text }],
                                    (keys::CITATIONS): cits
                                }
                            }));
                        }
                    }
                    crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        content_arr.push(serde_json::json!({(super::TOOL_USE_CAMEL): {(super::TOOL_USE_ID): id, (keys::NAME): name, (keys::INPUT): input}}));
                    }
                    crate::codec::ir::IrBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        let mut inner_content: Vec<serde_json::Value> = Vec::new();
                        for inner_block in content {
                            match inner_block {
                                b @ crate::codec::ir::IrBlock::Text { .. }
                                    if b.as_search_result().is_some() =>
                                {
                                    if let Some(sr) = b.as_search_result() {
                                        inner_content.push(super::write_bedrock_search_result(&sr));
                                    }
                                }
                                crate::codec::ir::IrBlock::Text { text, .. } => {
                                    inner_content.push(serde_json::json!({ (keys::TEXT): text }));
                                }
                                // Converse supports structured tool-result content as `{"json":
                                // <value>}` (inverse of `read_request`), so the content is preserved
                                // rather than collapsed to `"{}"`. A provider-run tool's record has no
                                // tool-result form: dropped on the drop path.
                                crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::HOSTED_TOOL,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [],
                                        "dropping a hosted tool record inside a Bedrock toolResult: \
                                         Converse has no form for it"
                                    );
                                }
                                crate::codec::ir::IrBlock::Json(value) => {
                                    // A structured-json block re-emits as a native `{"json": <value>}`.
                                    inner_content.push(serde_json::json!({ (keys::JSON): value }));
                                }
                                crate::codec::ir::IrBlock::Image { source, .. } => {
                                    if let Some(image_block) = bedrock_image_block(source) {
                                        inner_content.push(
                                            serde_json::json!({ (keys::IMAGE): image_block }),
                                        );
                                    }
                                }
                                crate::codec::ir::IrBlock::ToolUse {
                                    id, name, input, ..
                                } => {
                                    // A nested ToolUse has no native tool-result shape: carried as a
                                    // `json` block rather than discarding the call identity.
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): id, (keys::NAME): name, (keys::INPUT): input }
                                    }));
                                }
                                crate::codec::ir::IrBlock::ToolResult {
                                    tool_use_id,
                                    is_error,
                                    ..
                                } => {
                                    // A tool result nested in a tool result is not native: preserved as
                                    // a `json` block, not a meaningless `"{}"`.
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): tool_use_id, "isError": is_error }
                                    }));
                                }
                                // Thinking has no tool-result shape and carries no result data: omitted
                                // with a trace.
                                crate::codec::ir::IrBlock::Thinking { .. } => {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::THINKING,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [],
                                        "dropping non-representable Thinking block inside a Bedrock toolResult");
                                }
                                // `ToolResultContentBlock` is {json, text, image, document, video}, so
                                // a tool-returned attachment projects natively here too.
                                crate::codec::ir::IrBlock::Media {
                                    kind,
                                    source,
                                    name,
                                    citations,
                                    context,
                                    ..
                                } => {
                                    if let Some(b) = bedrock_media_content_block(
                                        *kind,
                                        source,
                                        name.as_deref(),
                                        *citations,
                                        context.as_deref(),
                                    ) {
                                        inner_content.push(b);
                                    }
                                }
                            }
                        }

                        let status_str = if *is_error {
                            keys::ERROR_WORD
                        } else {
                            "success"
                        };
                        content_arr.push(serde_json::json!({(super::TOOL_RESULT): {(super::TOOL_USE_ID): tool_use_id, (keys::CONTENT): inner_content, (keys::STATUS): status_str}}));
                    }
                    crate::codec::ir::IrBlock::Image { source, .. } => {
                        if let Some(image_block) = bedrock_image_block(source) {
                            content_arr.push(serde_json::json!({ (keys::IMAGE): image_block }));
                        }
                    }
                    crate::codec::ir::IrBlock::Thinking {
                        text,
                        signature,
                        redacted,
                        signature_origin,
                        ..
                    } => {
                        // Re-emit the assistant turn's reasoning as a native `reasoningContent` block
                        // (inverse of `read_request`): Bedrock requires the signed reasoning echoed
                        // back on a follow-up turn. A redacted block gives `redactedContent`, else
                        // `reasoningText`. IR-18: a signature another family minted (Gemini
                        // `thoughtSignature`, OpenAI `encrypted_content`) is a foreign blob a Bedrock
                        // model rejects, so only a Claude, Bedrock or unknown-origin one is sent.
                        let foreign = matches!(
                            signature_origin,
                            Some(
                                crate::codec::ir::IrSignatureOrigin::Gemini
                                    | crate::codec::ir::IrSignatureOrigin::OpenAi
                            )
                        );
                        if foreign && signature.is_some() {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::THINKING,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [origin = ?signature_origin, ],
                                "dropping a reasoning signature on Bedrock egress: another model \
                                 family minted it");
                        }
                        let signature = if foreign { &None } else { signature };
                        content_arr.push(bedrock_reasoning_block(text, signature, *redacted));
                    }
                    crate::codec::ir::IrBlock::Media {
                        kind,
                        source,
                        name,
                        citations,
                        context,
                        ..
                    } => {
                        if let Some(b) = bedrock_media_content_block(
                            *kind,
                            source,
                            name.as_deref(),
                            *citations,
                            context.as_deref(),
                        ) {
                            content_arr.push(b);
                        }
                    }
                    crate::codec::ir::IrBlock::Json(_)
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                        // Structured JSON is only a tool-result member; omitted from a message turn.
                    }
                }
                // Emit the cache boundary as a `cachePoint` right after its block; none if the block
                // wrote nothing (e.g. a dropped Image). Suppressed when the stash owns placement.
                if emit_inline_message_cache
                    && block_cache_control.is_some()
                    && content_arr.len() > written_before
                {
                    content_arr.push(bedrock_cache_point());
                }
            }

            // Re-emit captured `cachePoint` / `guardContent` / `document` / `video` markers for this
            // message at their original positions, before the empty-content placeholder below so a
            // marker-only message re-emits the marker. `msg_idx` matches the reader's index on the
            // passthrough path (the reader emits only User/Assistant turns, so no System `continue`
            // desyncs it). All classes are spliced as one sorted batch (same original array).
            let for_this_msg: Vec<serde_json::Value> = message_cache_points
                .into_iter()
                .chain(message_guard_content)
                .chain(message_doc_video)
                .flatten()
                .filter(|e| e.get(keys::M).and_then(|v| v.as_u64()) == Some(msg_idx as u64))
                .cloned()
                .collect();
            if !for_this_msg.is_empty() {
                splice_cache_points(&mut content_arr, &for_this_msg);
            }

            // Converse requires strictly alternating roles (consecutive same-role is a 400). After the
            // Tool to "user" mapping, IR shapes yield consecutive "user" turns (a Tool-result then a
            // user turn, or several Tool messages). Coalesce into the previous message on a shared
            // role. A same-protocol passthrough already alternates, so byte-identity holds.
            if let Some(prev_content) = msgs_arr
                .last_mut()
                .filter(|last| last.get(keys::ROLE).and_then(|r| r.as_str()) == Some(role_str))
                .and_then(|last| last.get_mut(keys::CONTENT))
                .and_then(|c| c.as_array_mut())
            {
                // Append to the previous same-role message; an empty `content_arr` appends nothing.
                prev_content.append(&mut content_arr);
                continue;
            }

            // A turn whose blocks were all non-representable (e.g. thinking-only) would yield an empty
            // `content_arr`; dropping it breaks Converse's alternation (400). Mirror the Anthropic
            // writer (which emits `""` for empty content) with a minimal placeholder text block.
            // System-role messages never reach here.
            if content_arr.is_empty() {
                content_arr.push(serde_json::json!({ (keys::TEXT): "" }));
            }
            let mut msg_obj = serde_json::Map::new();
            msg_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));
            msg_obj.insert(
                keys::CONTENT.to_string(),
                serde_json::Value::Array(content_arr),
            );
            msgs_arr.push(serde_json::Value::Object(msg_obj));
        }

        if !msgs_arr.is_empty() {
            out.insert(
                keys::MESSAGES.to_string(),
                serde_json::Value::Array(msgs_arr),
            );
        }

        // A cross-protocol reasoning ask is dropped with a warn for every model family (no Converse
        // mapping). Writing it as Claude's `additionalModelRequestFields.thinking` would switch on
        // billed thinking and change sampling controls and forced tool choice. A raw native ask
        // already in `extra` is re-emitted verbatim below.
        if req.reasoning.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::REASONING,
                &crate::codec::diagnostics::IR_DROP_REASONING,
                [],
                "dropping cross-protocol reasoning/thinking ask on Bedrock egress: no Bedrock \
                 Converse mapping, as in 1.5.5"
            );
        }

        // Rebuild `inferenceConfig` by overlaying the typed `maxTokens`/`temperature` onto the raw
        // object captured in `extra`. This keeps unmodeled sub-fields (`stopSequences`, `topP`,
        // `topK`, `stopCriteria`, future AWS additions) on a same-protocol passthrough, and builds
        // from the IR alone cross-protocol. Typed fields win; the raw object is consumed here, not
        // re-emitted by the trailing extra-merge.
        let mut inference_config = req
            .extra
            .get(super::INFERENCE_CONFIG)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(max_tokens) = req.max_tokens {
            inference_config.insert(
                super::MAX_TOKENS_CAMEL.to_string(),
                serde_json::json!(max_tokens),
            );
        }
        // `inferenceConfig.{temperature, topP, stopSequences}` are map-file rows, overlaid on the raw
        // object (typed wins, so a same-protocol round-trip re-emits the same value). Temperature is
        // clamped to Converse's [0.0, 1.0].
        out.insert(
            super::INFERENCE_CONFIG.to_string(),
            serde_json::Value::Object(inference_config),
        );
        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress { thinking: false },
            &mut out,
        );
        if out
            .get(super::INFERENCE_CONFIG)
            .and_then(|v| v.as_object())
            .is_some_and(serde_json::Map::is_empty)
        {
            out.remove(super::INFERENCE_CONFIG);
        }

        // response_format. BED-08: Converse carries structured output natively as
        // `outputConfig.textFormat` (`{type: "json_schema", structure: {jsonSchema: {schema:
        // "<string>", name, description}}}`). A JSON-schema directive projects onto it, overlaid on any
        // raw `outputConfig` in `extra` (a same-protocol body's own `textFormat` wins, being what the
        // typed field was read from). Forms the `json_schema`-only enum cannot express (schema-less
        // JSON, plain text) are dropped observably, so the divergence is not silent (mirrors the
        // Anthropic egress). Same-protocol relays the raw body and never reaches here.
        let mut output_config = req
            .extra
            .get(super::OUTPUT_CONFIG_CAMEL)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        // Whether the native form may be sent is a lane capability (`LaneCaps::native_structured_output`,
        // declared per model in the catalog): Converse accepts `outputConfig.textFormat` only on the
        // models AWS lists and 400s on the rest. A lane that does not declare it drops the directive, observably.
        if let Some(rf) = &req.response_format {
            match super::write_bedrock_text_format(rf) {
                // A same-protocol body's own raw `textFormat` is already in place and wins.
                _ if output_config.contains_key(super::TEXT_FORMAT) => {}
                Some(tf) if caps.native_structured_output => {
                    output_config.entry(super::TEXT_FORMAT).or_insert(tf);
                }
                Some(_) => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::RESPONSE_FORMAT,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [parameter = keys::RESPONSE_FORMAT,],
                        "dropping response_format on Bedrock egress: the lane does not declare \
                         native structured outputs (`native_structured_output`), and Converse \
                         rejects `outputConfig.textFormat` on a model without them"
                    );
                }
                None => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::RESPONSE_FORMAT,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [parameter = keys::RESPONSE_FORMAT,],
                        "dropping response_format on Bedrock egress: Converse's \
                         `outputConfig.textFormat` models only a JSON schema, and this directive \
                         carries none (schema-less JSON mode or plain text)"
                    );
                }
            }
        }
        if !output_config.is_empty() {
            out.insert(
                super::OUTPUT_CONFIG_CAMEL.to_string(),
                serde_json::Value::Object(output_config),
            );
        }

        // Rebuild `toolConfig` by overlaying the typed `tools` onto the raw object captured in
        // `extra`. This keeps unmodeled sub-fields (notably `toolChoice`, the force-tool-use control)
        // on a same-protocol passthrough, and builds from the IR alone cross-protocol. Typed `tools`
        // win; the raw object is consumed here, not re-emitted by the trailing extra-merge.
        //
        // Emitted only with something to emit (typed tools or a non-empty raw object): AWS rejects a
        // `toolConfig` with an empty `tools` array, so never a bare `{}` / `{tools:[]}`.
        let mut tool_config = req
            .extra
            .get(keys::TOOL_CONFIG)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        // IR-10: an allowed-tools subset has no Converse directive; it is expressed by omission (only
        // the listed tools are sent) and the mode rides `toolChoice` (`Auto` to `auto`, `Required` to `any`).
        let sent_tools: Vec<&crate::codec::ir::IrTool> = match &req.allowed_tools {
            Some(allowed) => req
                .tools
                .iter()
                .filter(|t| allowed.contains(&t.name))
                .collect(),
            None => req.tools.iter().collect(),
        };
        if !sent_tools.is_empty() {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in sent_tools {
                let mut tool_spec = serde_json::Map::new();
                tool_spec.insert(keys::NAME.to_string(), serde_json::json!(tool.name));

                if let Some(desc) = &tool.description {
                    tool_spec.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }
                // BED-08: `toolSpec.strict`, the per-tool structured-output switch.
                if let Some(strict) = tool.strict {
                    tool_spec.insert(keys::STRICT.to_string(), serde_json::json!(strict));
                }

                let mut input_schema = serde_json::Map::new();
                input_schema.insert(keys::JSON.to_string(), tool.input_schema.clone());
                tool_spec.insert(
                    super::INPUT_SCHEMA_CAMEL.to_string(),
                    serde_json::Value::Object(input_schema),
                );

                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(
                    super::TOOL_SPEC.to_string(),
                    serde_json::Value::Object(tool_spec),
                );
                tools_arr.push(serde_json::Value::Object(tool_obj));

                // A tool-definition cache boundary is a `cachePoint` element in `toolConfig.tools`
                // right after the tool it closes. With no positional stash, the typed `cache_control`
                // is the sole carrier on both paths (the raw `toolConfig.tools` is clobbered by this
                // rebuild), so no suppression gate.
                if tool.cache_control.is_some() {
                    tools_arr.push(bedrock_cache_point());
                }
            }

            tool_config.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }
        // Emit `toolChoice` from the typed IR union. A same-protocol raw `toolConfig` still carries
        // the original key, so drop it first: the typed value is the single source of truth.
        // `IrToolChoice::None` has no native form, so nothing is emitted for it.
        tool_config.remove(super::TOOL_CHOICE_CAMEL);
        // `toolChoice` is valid only with a non-empty `tools` array (F3, ValidationException), so
        // emit the typed choice only when tools are present (typed or a raw `toolConfig.tools`). One
        // arriving with no surviving tools is dropped with a warn rather than emitted into an invalid body.
        if tool_config.contains_key(keys::TOOLS) {
            if let Some(tc) = &req.tool_choice {
                match write_bedrock_tool_choice(tc) {
                    Some(v) => {
                        tool_config.insert(super::TOOL_CHOICE_CAMEL.to_string(), v);
                    }
                    // `IrToolChoice::None` has no Converse directive, so `toolChoice` is omitted (the
                    // backend default may still call a tool). Warned so it is observable.
                    None => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::TOOL_CHOICE,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping tool_choice=None: Bedrock Converse has no 'do not call a tool' \
                             directive, so toolChoice is omitted and the backend may still call a tool");
                    }
                }
            }
        } else if req.tool_choice.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::TOOL_CHOICE,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping tool_choice with no accompanying tools: Bedrock Converse rejects a \
                 toolConfig whose toolChoice has no tools array, so it is omitted"
            );
        }
        // Emit `toolConfig` only with a `tools` array: AWS rejects `{}`, `{tools:[]}` and a
        // `toolChoice` with no tools.
        if tool_config.contains_key(keys::TOOLS) {
            out.insert(
                keys::TOOL_CONFIG.to_string(),
                serde_json::Value::Object(tool_config),
            );
        }
        // Converse models no parallelism control; `is_some()` gates this to requests that carried the flag.
        if req.parallel_tool_calls.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::PARALLEL_TOOL_CALLS,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping parallel_tool_calls on Bedrock egress: Converse has no parallelism \
                 control, so the backend's default parallelism applies"
            );
        }

        // Emit `top_k`. Converse has no `inferenceConfig` slot for it; it rides model-specific
        // `additionalModelRequestFields`. The typed `top_k` is overlaid onto the raw object captured
        // in `extra` (as for `inferenceConfig`/`toolConfig`), re-emitting a same-protocol top_k and
        // carrying a cross-protocol one (where `extra` is cleared). The raw object is consumed here.
        // The typed `top_k` wins.
        let mut additional_fields = req
            .extra
            .get(super::ADDITIONAL_MODEL_REQUEST_FIELDS)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(top_k) = req.top_k {
            // Preserve the source spelling: camelCase `topK` when the reader stamped the sentinel,
            // else `top_k`. The sentinel survives only same-protocol, so cross-protocol emits `top_k`.
            let key = if req.extra.contains_key(TOP_K_CAMEL_SENTINEL) {
                super::TOP_K_CAMEL
            } else {
                super::TOP_K
            };
            additional_fields.insert(key.to_string(), serde_json::json!(top_k));
        }
        if !additional_fields.is_empty() {
            out.insert(
                super::ADDITIONAL_MODEL_REQUEST_FIELDS.to_string(),
                serde_json::Value::Object(additional_fields),
            );
        }

        // IR-03: typed metadata is Converse's `requestMetadata`. A same-protocol raw object in
        // `extra` (re-emitted verbatim below) is what it was read from and wins.
        if let Some(pairs) = req
            .metadata
            .as_deref()
            .filter(|_| !req.extra.contains_key(super::FIELD_REQUEST_METADATA))
        {
            if let Some(m) = super::write_bedrock_request_metadata(pairs) {
                out.insert(super::FIELD_REQUEST_METADATA.to_string(), m);
            }
        }
        // The typed request slots with no Converse form, the same set `dropped_egress_controls` reports.
        crate::codec::carry::warn_drops(
            super::map::REQUEST,
            super::map::CONTROLS,
            Some(&super::map::DROP_WARN),
            req,
        );
        crate::codec::dialect::warn_dropped(
            req.hosted_tools.iter().map(|h| h.kind_str()),
            &super::map::DROP_WARN,
        );

        for (key, value) in &req.extra {
            // `inferenceConfig` and `toolConfig` were consumed above (typed fields overlaid on the raw
            // object); re-inserting the raw copy would clobber the overlay. Other unmodeled fields
            // pass through verbatim.
            if key == super::INFERENCE_CONFIG
                || key == keys::TOOL_CONFIG
                || key == super::OUTPUT_CONFIG_CAMEL
            {
                continue;
            }
            // `additionalModelRequestFields` was consumed above (typed `top_k` overlaid); re-inserting
            // would clobber it. Skipped (mirrors inferenceConfig/toolConfig).
            if key == super::ADDITIONAL_MODEL_REQUEST_FIELDS {
                continue;
            }
            // The cachePoint stash is an internal sentinel, already spliced into `system`/`messages`;
            // emitting it would leak it onto the wire (an invalid body and a proxy tell).
            if key == CACHE_POINTS_SENTINEL {
                continue;
            }
            // The guardContent stash is likewise an internal sentinel, already consumed.
            if key == GUARD_CONTENT_SENTINEL {
                continue;
            }
            // The document/video stash is likewise an internal sentinel, already consumed.
            if key == DOC_VIDEO_SENTINEL {
                continue;
            }
            // The top_k spelling hint is an internal sentinel, already consumed (it chose the key in
            // `additionalModelRequestFields`); skipped so it never leaks onto the wire.
            if key == TOP_K_CAMEL_SENTINEL {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        serde_json::Value::Object(out)
    }
}
