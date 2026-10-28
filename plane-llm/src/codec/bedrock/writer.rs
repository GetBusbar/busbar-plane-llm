use super::*;
use crate::codec::keys;

/// AWS Converse `usage.totalTokens`: every token the call consumed, cache tokens included.
///
/// Under prompt caching AWS reports `inputTokens` as the non-cached input only; the full input is
/// `inputTokens + cacheReadInputTokens + cacheWriteInputTokens` (its worked example totals
/// `inputTokens: 16, outputTokens: 4, cacheWriteInputTokens: 695` as `totalTokens: 715`).
/// `write_cache_usage` emits the cache counts as additive siblings, so leaving them out of the total
/// would publish parts that sum past it and the tokens would vanish from an accounting consumer's
/// books. The reader never reads `totalTokens`, so this is the only place the wire total is
/// decided, for both the buffered body and the streamed `metadata` frame.
///
/// All adds are `saturating_add`: the operands are upstream-controlled, and a bare `+` near
/// `u64::MAX` would panic request-path code under overflow-checks or wrap to nonsense in release.
/// Mirrors the Gemini writer.
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

    // The model rides the URL, not the body: a no-op that always reports `false`, so model
    // rewriting never makes a same-protocol passthrough non-pristine.
    fn rewrite_model_if_needed(&self, _body: &mut serde_json::Value, _model: &str) -> bool {
        false
    }

    // `inferenceConfig.maxTokens` is optional (the model's default applies, and an empty
    // `inferenceConfig` is omitted), so `requires_max_tokens` is not overridden: injecting a
    // default would silently cap output.

    /// Converse frames a turn as `{role, content: [{text}]}`; a verbatim insert of the reply's
    /// `{role, content: "…"}` messages would corrupt that shape (it spells its container
    /// `messages` too), so this override is load-bearing.
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
        // A JSON-schema directive projects onto `outputConfig.textFormat` on a lane declaring native
        // structured outputs; any other directive, and every one on other lanes, is dropped.
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

    /// The production write: the lane's declared [`LaneCaps`] choose the structured-output form
    /// (see `write_request_with`). A cross-protocol reasoning ask is dropped for every model family
    /// there.
    fn write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        self.write_request_with(req, caps)
    }

    fn write_request(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
    ) -> serde_json::Value {
        self.write_request_with(req, &LaneCaps::default())
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
                // A text block has no `contentBlockStart` on the wire: `ContentBlockStart$start` is a
                // union of `toolUse` (plus `image`/`toolResult` in newer revisions), with no text
                // member. A real stream opens text implicitly with the first `contentBlockDelta`
                // carrying `text` and closes it with `contentBlockStop`, as the reader's lazy open
                // mirrors; an empty `start: {}` is off-spec and a tell. `mark_block_open` still fires
                // so the `BlockStop` emits `contentBlockStop`; no start frame.
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
                // A reasoning block has no `contentBlockStart` either (the start union models only
                // `toolUse`). Reasoning streams entirely through
                // `contentBlockDelta.delta.reasoningContent` (`text` / `signature` /
                // `redactedContent`) and closes with `contentBlockStop`, as the reader's lazy open
                // mirrors; a `start.reasoningContent` frame is off-spec and a tell. Plaintext and
                // redacted reasoning behave alike: no start frame, but `mark_block_open` still fires
                // so the `BlockStop` emits; opaque bytes ride the following `RedactedReasoningDelta`
                // under `redactedContent`. (Image has no streaming-start projection and is not
                // marked open, so it emits no orphan stop.)
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

                // Streamed extended thinking. `ReasoningContentBlockDelta` carries one of `text`,
                // `signature` or `redactedContent` per frame, so each IR delta maps to exactly one
                // frame. The streaming inverse of `bedrock_reasoning_block`.
                crate::codec::ir::IrDelta::ThinkingDelta(text) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (keys::TEXT): text } }
                    }),
                )),

                // A genuine reasoning signature token re-emits under `signature`.
                crate::codec::ir::IrDelta::SignatureDelta(sig) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (keys::SIGNATURE): sig } }
                    }),
                )),
                // A redacted-reasoning delta re-emits the bytes under `redactedContent`, never as a
                // plaintext `signature`. The stream has no generated-media delta member.
                crate::codec::ir::IrDelta::MediaDelta(_) => None,
                crate::codec::ir::IrDelta::RedactedReasoningDelta(redacted) => Some((
                    ET_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::json!({
                        (super::CONTENT_BLOCK_INDEX): index,
                        (keys::DELTA): { (super::REASONING_CONTENT): { (super::REDACTED_CONTENT): redacted } }
                    }),
                )),
                // Streamed grounding citations. `ContentBlockDelta` has a `citation` member (a
                // `CitationsDelta` with the buffered `Citation`'s `{title, sourceContent, location}`),
                // interleaved with `text` deltas at the same `contentBlockIndex`, so sources appear at
                // `stream: true` as at `stream: false`.
                //
                // A native `citation` delta carries one citation, so this single-frame arm frames the
                // first of the batch; `write_response_events` walks a multi-citation delta, re-entering
                // once per citation. A direct caller (outside the framing seam's
                // `max_citations_per_delta` fan-out) still gets a well-formed frame.
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
                        // Every neutral field was empty, so no union member exists and `{}` would be
                        // malformed: drop it, with a warn.
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
                // Bedrock Converse has no logprobs shape; dropped.
                crate::codec::ir::IrDelta::LogprobsDelta(_) => None,
            },

            // An untracked index is a block whose start had no projection (Image); closing it would
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

            // The wire carries `stopReason` in a `messageStop` frame and `usage` in a separate
            // `metadata` frame after it, while the IR carries one combined
            // `MessageDelta{stop_reason, usage}`. One `(event_type, json)` return cannot emit two
            // frames, so the Bedrock-ingress fan-out lives in `StreamTranslate::translate_event`,
            // which splits the combined delta and injects the real `metrics.latencyMs` on `metadata`.
            //
            // This arm maps each already-split MessageDelta to its single frame:
            //   - stop_reason = Some(...)  → `messageStop` (usage ignored)
            //   - stop_reason = None       → `metadata` with the real usage (no `metrics`: the
            //                                fan-out adds the real wall-clock or omits it; a
            //                                fabricated `latencyMs: 0` is a tell)
            // The stream has no stop_sequence field, so it is ignored. A context-window stop detail is
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
                    // Cache-inclusive and saturating — see `converse_total_tokens`.
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

            // A mid-stream error on Bedrock ingress is natively a modeled-exception event-stream frame
            // (`:message-type: exception` + `:exception-type`), which `StreamTranslate` emits via
            // `write_response_exception` + `eventstream::encode_exception_frame` before this arm
            // (an `Error` never routes through `write_response_event` there). This arm fires only if
            // a non-eventstream consumer drives the writer; it falls back to an `event`-typed frame
            // naming a stream-union exception (`bedrock_stream_exception_for`, not the request-level
            // set), never the literal `"error"`.
            IrStreamEvent::Error(err) => {
                let (exception_name, message) = bedrock_stream_exception_for(err);
                Some((
                    exception_name.to_string(),
                    serde_json::json!({ (keys::MESSAGE): message }),
                ))
            }
        }
    }

    /// A `citation` delta carries one citation, so a delta holding several frames as several events
    /// at the same `contentBlockIndex`, as a native stream interleaves them. The framing seam splits
    /// them (`max_citations_per_delta`), but the plane codec also drives `write_response_events`
    /// directly, and a whole batch must emit all its citations, not keep the first. Each re-enters
    /// the single-frame arm with a one-citation delta, keeping that arm the one source of the
    /// `citation` shape. Every other event keeps the base wrapper's one-frame behaviour.
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

    /// A Bedrock-ingress mid-stream error is a modeled-exception event-stream frame
    /// (`:message-type: exception`) emitted by `StreamTranslate` via
    /// `eventstream::encode_exception_frame`. This maps the IR error to its
    /// `(exception_name, message)` with `bedrock_stream_exception_for`, the five-member stream
    /// union rather than the larger request-level set, so an SDK stream decoder recognizes the
    /// `:exception-type`. Shared with the fallback `write_response_event` Error arm.
    fn write_response_exception(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        err: &busbar_contract::protocol::IrError,
    ) -> Option<(String, String)> {
        let (exception_name, message) = bedrock_stream_exception_for(err);
        Some((exception_name.to_string(), message))
    }

    fn write_error_frame(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &busbar_contract::protocol::IrError,
    ) -> Option<(String, serde_json::Value)> {
        // A Bedrock-ingress stream never reaches here (its error is the exception frame from
        // `write_response_exception`); this serves a non-eventstream consumer and delegates to the
        // `write_response_event` Error arm so the two stay byte-identical.
        self.write_response_event(host, &IrStreamEvent::Error(err.clone()))
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
                    // Grounding citations in Converse's `citationsContent` slot, the buffered twin of
                    // the streaming `contentBlockDelta.citation` frame, so a Bedrock-ingress client
                    // gets sources whether or not it streams.
                    //
                    // `citationsContent` replaces the plain `text` member (it carries the text
                    // inside), so it is emitted only when at least one citation projects; an
                    // uncited block keeps the plain `{"text": …}` shape.
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

                // A model does not emit an attachment back on the Converse response surface.
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
                    // An assistant response can carry an Image block (a cross-protocol source that
                    // emitted one), natively an `{"image": ...}` block. A source with no projection
                    // (URL / file_id) returns `None` and is omitted with a trace.
                    if let Some(image_block) = bedrock_image_block(source) {
                        content_arr.push(serde_json::json!({ (keys::IMAGE): image_block }));
                    }
                }
                crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                    // Structured-json is only a tool-result member; omitted from an assistant turn.
                }

                crate::codec::ir::IrBlock::Thinking {
                    text,
                    signature,
                    redacted,
                    ..
                } => {
                    // Re-emit the reasoning as a native `reasoningContent` block (the inverse of
                    // `read_response`'s decode): redacted re-emits `redactedContent`, any other
                    // Thinking `reasoningText`.
                    content_arr.push(bedrock_reasoning_block(text, signature, *redacted));
                }

                // A `toolResult` is a user-turn block with no place in an assistant response: the
                // only genuine no-op here.
                crate::codec::ir::IrBlock::ToolResult { .. } => {}
            }
        }

        // Converse rejects an assistant message with an empty `content` array (ValidationException),
        // as `write_request` guards. A response whose blocks were all non-representable would emit
        // `content: []`, so a minimal placeholder text block keeps the body valid.
        if content_arr.is_empty() {
            content_arr.push(serde_json::json!({ (keys::TEXT): "" }));
        }

        // A context-window detail is Converse's own `model_context_window_exceeded`.
        let reverse_reason = stop_reason_reverse_detailed(
            resp.stop_reason
                .unwrap_or(crate::codec::ir::IrStopReason::EndTurn),
            resp.stop_detail.as_ref(),
        );

        // No identity is emitted. The native body (`output` / `stopReason` / `usage` / optional
        // `metrics`) carries no id or `created`; the request id is only the `x-amzn-RequestId`
        // header. A synthesized body `id` would be a tell, so none is added. (A Bedrock egress
        // feeding an ingress that needs a body id is that ingress writer's job.) `stopReason` and
        // `usage` are reproduced exactly from the IR, so a same-protocol round-trip is byte-identical.
        let mut usage_obj = serde_json::Map::new();
        usage_obj.insert(
            super::INPUT_TOKENS_CAMEL.to_string(),
            resp.usage.input_tokens.into(),
        );
        usage_obj.insert(
            super::OUTPUT_TOKENS_CAMEL.to_string(),
            resp.usage.output_tokens.into(),
        );
        // Cache-inclusive and saturating, as the streaming `metadata` frame (see
        // `converse_total_tokens`); this keeps the round-trip byte-identical when cache tokens
        // were reported.
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

    /// Native Converse error envelope: a flat body with the detail in a lowercase `"message"` and
    /// the exception name in `"__type"` (the two fields `BedrockReader::extract_error` reads back).
    /// An SDK builds the typed exception from `__type`, so the generic `{"error":{...}}` envelope
    /// would fail to decode. The generic `kind` maps to the closed AWS exception set via
    /// `error_kind_to_bedrock_type`. Served as `application/json`.
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
        // A real response always carries `x-amzn-RequestId` (the SDK's only request-id surface) and
        // `x-amzn-errortype` equal to the body `__type`; omitting them is a tell and leaves the SDK
        // request id empty on the most-exercised failover error surface.
        attach_bedrock_error_headers(host, headers, kind);
    }

    fn new_stream_framing(&self) -> Box<dyn super::StreamFraming> {
        // Ingress framing: the messageStop/metadata deferral and exactly-one-metadata invariant,
        // so the agnostic translator names no Bedrock wire shape.
        Box::<BedrockStreamFraming>::default()
    }

    fn wrap_buffered_as_stream(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ir: &crate::codec::ir::IrResponse,
        elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        // A client that requested ConverseStream but got a buffered 2xx must receive a native
        // eventstream frame sequence, not a JSON body the SDK cannot parse. Delegates to the
        // module-local fn that synthesizes the frames through this same writer.
        Some(bedrock_response_to_eventstream(host, ir, elapsed_ms))
    }

    fn inject_response_metrics(&self, value: &mut serde_json::Value, elapsed_ms: Option<u64>) {
        // A native non-stream response always populates the required `metrics.latencyMs`.
        // `write_response` omits it (timing is unknown there); inject the real elapsed wall-clock
        // here. A well-formed `metrics` is kept, and the member is emitted even without timing, as
        // for the streaming `metadata` frame.
        super::ensure_metrics(value, elapsed_ms);
    }

    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        // A real response carries `x-amzn-RequestId`: the captured upstream id verbatim on a
        // same-protocol passthrough, else synthesized (non-stream and cross-protocol supply
        // `None`). Synthesis failure (no entropy) omits the header rather than panicking.
        upstream_request_id
            .map(String::from)
            .or_else(|| synth_amzn_request_id(host))
            .map(|id| (HDR_AMZN_REQUEST_ID, id))
    }

    /// A `reasoningText.signature` is read back as Claude's or the Bedrock model's own (the reader
    /// tells them apart by the conversation's model id).
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

        // The captured `cachePoint` markers (`CACHE_POINTS_SENTINEL`): present on a same-protocol
        // passthrough, absent cross-protocol where `extra` is cleared (no Bedrock-only marker
        // leaks). The `system`/`messages` sub-arrays are spliced back below and the trailing
        // extra-merge skips the sentinel, so it never reaches the wire.
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

        // The captured `guardContent` markers (`GUARD_CONTENT_SENTINEL`), same shape and splice
        // helper; consumed here and skipped by the trailing extra-merge.
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

        // The captured `document` / `video` markers (`DOC_VIDEO_SENTINEL`), same shape and helper;
        // consumed here and skipped by the extra-merge. Only messages carry them, not `system`.
        let doc_video = req
            .extra
            .get(DOC_VIDEO_SENTINEL)
            .and_then(|v| v.as_object());
        let message_doc_video = doc_video
            .and_then(|dv| dv.get(keys::MESSAGES))
            .and_then(|v| v.as_array());

        // When the positional cachePoint stash is present (same-protocol) it owns placement
        // (spliced below at the recorded indices, byte-identical), so inline `cache_control`
        // emission is suppressed to avoid a double marker. Cross-protocol, `extra` is cleared and the
        // inline emission is the sole carrier, projecting an Anthropic breakpoint onto a cachePoint.
        let emit_inline_system_cache = system_cache_points.is_none();
        if !req.system.is_empty() || system_cache_points.is_some() || system_guard_content.is_some()
        {
            let mut text_arr: Vec<serde_json::Value> = Vec::new();
            // System blocks the reader modelled out of a stashed `guardContent`: the splice below
            // re-emits the verbatim block, so the modelled copy is not written.
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
                    // A `cachePoint` after the block carrying the `cache_control` boundary (the
                    // breakpoint closes the prefix before it), suppressed when the stash owns placement.
                    if emit_inline_system_cache && cache_control.is_some() {
                        text_arr.push(bedrock_cache_point());
                    }
                }
            }

            // Re-emit captured `cachePoint` / `guardContent` markers at their original positions so
            // prompt caching and inline guardrails survive a same-protocol round-trip. Both recorded
            // indices against the same original array, so they splice as one sorted batch (two passes
            // would shift the second's indices); `merge_marker_entries` concatenates the lists.
            let merged = merge_marker_entries(system_cache_points, system_guard_content);
            splice_cache_points(&mut text_arr, &merged);

            if !text_arr.is_empty() {
                out.insert(keys::SYSTEM.to_string(), serde_json::Value::Array(text_arr));
            }
        }

        // Same suppression gate as the system array: a present message-cachePoint stash owns
        // placement; cross-protocol (stash cleared) inline emission is the sole carrier.
        let emit_inline_message_cache = message_cache_points.is_none();
        let mut msgs_arr: Vec<serde_json::Value> = Vec::new();
        for (msg_idx, msg) in req.messages.iter().enumerate() {
            let role_str = match msg.role {
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                // Converse has no "tool" role: a tool result is a `toolResult` block in a user-turn
                // message, so Tool maps to "user".
                crate::codec::ir::IrRole::Tool => keys::USER,
                // System text is extracted into the top-level `system` array above, so a System
                // message should not reach the wire. One that escapes is skipped, not mislabeled
                // "user", which would inject system instructions as a user turn.
                crate::codec::ir::IrRole::System => continue,
            };

            let mut content_arr: Vec<serde_json::Value> = Vec::new();
            // IR blocks the reader modelled out of a wire block it also parked verbatim (`document` /
            // `video` / `guardContent`), keyed by the reader's IR index `b`.
            let stashed_here =
                stashed_ir_indices([message_doc_video, message_guard_content], Some(msg_idx));
            for (block_idx, block) in msg.content.iter().enumerate() {
                // A block the reader also parked verbatim (`DOC_VIDEO_SENTINEL` or
                // `GUARD_CONTENT_SENTINEL`) at this (message, block) position. The stash is spliced
                // back below, so writing the modelled projection too would emit it twice. The
                // verbatim block wins, keeping a Bedrock-to-Bedrock round-trip byte-identical
                // (`citations`, `context`, guard `qualifiers` survive); a cross-protocol IR has no
                // stash and takes the modelled path.
                // Matched on `b`, the IR index the reader recorded, not `i`, the wire slot it is
                // spliced at. They differ by the wire-only blocks (`cachePoint`) before it;
                // comparing the wire index here missed the match and emitted the attachment twice.
                if stashed_here.contains(&block_idx) {
                    continue;
                }
                // The prompt-cache boundary on this block, if any, emitted as a `cachePoint` right
                // after it, suppressed when the stash owns placement. Thinking / Image / Media carry
                // one too (an Anthropic breakpoint), so they project a `cachePoint` like Text.
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
                // A block's projection may be nothing (an image with no source, audio); a cachePoint
                // is placed only after a block actually written.
                let written_before = content_arr.len();
                match block {
                    // An empty text block carrying only citations: Converse rejects blank text, so the
                    // carrier is omitted.
                    // The search-result slot: a RAG passage (from an Anthropic `search_result`, or a
                    // Converse one whose parked block is absent) re-emits as `searchResult`.
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
                        // A cited assistant-history text block is `citationsContent` (the text inside it
                        // beside its citations), as the response writer. Uncited text keeps `{text}`.
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
                                // Bedrock Converse natively supports structured tool-result content
                                // via a `{"json": <value>}` block (the inverse of what `read_request`
                                // decodes). Preserve the actual content instead of collapsing it to
                                // the constant string `"{}"`: a JSON-string Text-equivalent or a
                                // structured result that arrives via the IR is re-encoded faithfully.
                                // A provider-run tool's record has no tool-result content form:
                                // dropped on the drop path, never silently.
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
                                    // A structured-json tool-result block re-emits as a native
                                    // `{"json": <value>}` block, restoring same-protocol fidelity.
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
                                    // Nested ToolUse inside a tool result has no native Bedrock
                                    // tool-result shape; carry it as a structured `json` block rather
                                    // than discarding the call identity.
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): id, (keys::NAME): name, (keys::INPUT): input }
                                    }));
                                }
                                crate::codec::ir::IrBlock::ToolResult {
                                    tool_use_id,
                                    is_error,
                                    ..
                                } => {
                                    // A tool result nested in a tool result is not a native shape;
                                    // keep its identity as a `json` block, not a `"{}"` placeholder.
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): tool_use_id, "isError": is_error }
                                    }));
                                }
                                // Thinking has no tool-result shape and no result data: omitted with a
                                // trace, not a misleading placeholder.
                                crate::codec::ir::IrBlock::Thinking { .. } => {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::THINKING,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [],
                                        "dropping non-representable Thinking block inside a Bedrock toolResult");
                                }
                                // `ToolResultContentBlock` is {json, text, image, document, video}, the
                                // same document/video members as the top level, so a tool-returned
                                // attachment projects natively.
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
                        // Re-emit the assistant turn's reasoning as a `reasoningContent` block (the
                        // inverse of `read_request`'s decode); Bedrock requires the signed reasoning
                        // echoed on a follow-up turn. Redacted re-emits `redactedContent`, any other
                        // Thinking `reasoningText`. A signature another family minted (Gemini
                        // `thoughtSignature`, OpenAI `encrypted_content`) is rejected by a Bedrock
                        // model, so only a Claude, Bedrock or unknown-origin one is sent.
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
                        // Structured-json is only a tool-result member; omitted from a message turn.
                    }
                }
                // The prompt-cache boundary as a `cachePoint` right after its block (none if the block
                // wrote nothing), suppressed when the stash owns placement.
                if emit_inline_message_cache
                    && block_cache_control.is_some()
                    && content_arr.len() > written_before
                {
                    content_arr.push(bedrock_cache_point());
                }
            }

            // Re-emit this message's captured `cachePoint` / `guardContent` / `document` / `video`
            // markers at their original positions, spliced before the empty-content placeholder
            // below so a message whose only block was one re-emits it, not a bare `""`. `msg_idx`
            // matches the reader's index (it emits only User/Assistant turns, so no System-role
            // `continue` desyncs it). All classes splice as one sorted batch, since they recorded
            // indices against the same original array.
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

            // Converse requires strictly alternating user/assistant turns; two consecutive messages of
            // one role are a 400. After the Tool-to-"user" mapping, common IR shapes yield consecutive
            // "user" turns (a tool result then a real user turn, or several Tool messages:
            // assistant,user,user). This turn merges into the previous message when they share a
            // role. A same-protocol input already alternates, so this never fires there.
            if let Some(prev_content) = msgs_arr
                .last_mut()
                .filter(|last| last.get(keys::ROLE).and_then(|r| r.as_str()) == Some(role_str))
                .and_then(|last| last.get_mut(keys::CONTENT))
                .and_then(|c| c.as_array_mut())
            {
                // Merge: append this turn's blocks to the previous same-role message; an empty
                // content_arr appends nothing.
                prev_content.append(&mut content_arr);
                continue;
            }

            // A turn whose blocks were all non-representable (e.g. thinking-only) would yield an
            // empty `content_arr`. Dropping the message loses turn structure and can break the
            // alternation Converse enforces (a 400), so, as the Anthropic writer, a minimal
            // placeholder text block keeps the turn. System messages never reach here.
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

        // A cross-protocol reasoning ask is dropped with a warn for every model family (Converse has
        // no mapping). Writing it as `additionalModelRequestFields.thinking` would switch on billed
        // thinking and change sampling and forced tool choice. A raw native ask already in `extra`
        // (a same-protocol body) is re-emitted verbatim below.
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
        // object the reader captured into `extra`. That preserves every unmodeled sub-field
        // (`stopSequences`, `topP`, `topK`, `stopCriteria`, future additions) same-protocol, while a
        // cross-protocol egress (no raw `inferenceConfig`) builds purely from the IR. Typed fields
        // win over a same-named raw entry. The raw object is consumed here, not re-emitted by the
        // trailing extra-merge.
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
        // `inferenceConfig.{temperature, topP, stopSequences}` are mapping-file rows overlaid on the
        // raw object (typed IR wins; a same-protocol round-trip re-emits the same value).
        // Temperature is clamped to [0.0, 1.0].
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

        // There is no top-level `response_format`; structured output is native as
        // `outputConfig.textFormat` (`{type: "json_schema", structure: {jsonSchema: {schema:
        // "<string>", name, description}}}`). A JSON-schema directive projects onto it, overlaid on
        // any raw `outputConfig` in `extra` (a same-protocol body's own `textFormat` wins). Forms the
        // `json_schema`-only enum cannot express (schema-less JSON, plain text) are dropped with a
        // `warn!`, as in the Anthropic egress. Same-protocol relays raw bytes and never reaches
        // here, so this fires only for a cross-protocol IR.
        let mut output_config = req
            .extra
            .get(super::OUTPUT_CONFIG_CAMEL)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        // Whether the native form may be sent is a lane capability
        // (`LaneCaps::native_structured_output`): Converse accepts `textFormat` only on the models
        // AWS lists and 400s the rest. A lane that does not declare it drops the directive, observably.
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

        // Rebuild `toolConfig` by overlaying the typed `tools` onto the raw object the reader
        // captured into `extra`. That preserves unmodeled sub-fields, notably `toolChoice`
        // (the force-tool-use control), same-protocol, while a cross-protocol egress builds purely
        // from the IR. Typed `tools` win over a same-named raw entry, and the raw object is
        // consumed here, not re-emitted by the trailing extra-merge.
        //
        // `toolConfig` is emitted only when there is something to emit; AWS rejects an empty
        // `tools` array, so a bare `{}`/`{tools:[]}` is never written.
        let mut tool_config = req
            .extra
            .get(keys::TOOL_CONFIG)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        // An allowed-tools subset has no Converse directive; it is expressed by omission (only the
        // listed tools are sent) and the mode rides `toolChoice` (`Auto` is `auto`, `Required` is `any`).
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
                // `toolSpec.strict`, the per-tool structured-output switch.
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

                // A tool-definition cache boundary is a `cachePoint` in `toolConfig.tools` right after
                // the tool it closes. There is no positional tools stash, so the typed
                // `cache_control` is the sole carrier both same- and cross-protocol (the typed rebuild
                // clobbers the raw `tools`): no suppression gate.
                if tool.cache_control.is_some() {
                    tools_arr.push(bedrock_cache_point());
                }
            }

            tool_config.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }
        // `toolChoice` from the typed IR union. The raw `toolConfig` cloned from `extra` still carries
        // the original key, so drop it first: the typed value is the single source of truth.
        // `IrToolChoice::None` has no representation, so no `toolChoice` is emitted.
        tool_config.remove(super::TOOL_CHOICE_CAMEL);
        // `toolChoice` is valid only beside a non-empty `tools` array (Converse 400s otherwise), so
        // it is emitted only when tools are present (typed, or a raw `toolConfig.tools` from
        // same-protocol `extra`). One arriving with no surviving tools is dropped with a warn.
        if tool_config.contains_key(keys::TOOLS) {
            if let Some(tc) = &req.tool_choice {
                match write_bedrock_tool_choice(tc) {
                    Some(v) => {
                        tool_config.insert(super::TOOL_CHOICE_CAMEL.to_string(), v);
                    }
                    // `IrToolChoice::None` ("do NOT call a tool") has no Converse directive, so
                    // `toolChoice` is omitted (the backend default may still call a tool); warned.
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
        // Emit `toolConfig` only with a `tools` array; AWS rejects `{}`, `{tools:[]}` and a
        // `toolChoice` with no tools.
        if tool_config.contains_key(keys::TOOLS) {
            out.insert(
                keys::TOOL_CONFIG.to_string(),
                serde_json::Value::Object(tool_config),
            );
        }
        // Converse models no parallelism control; gated on `is_some()` so only requests that carried
        // the flag warn.
        if req.parallel_tool_calls.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::PARALLEL_TOOL_CALLS,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping parallel_tool_calls on Bedrock egress: Converse has no parallelism \
                 control, so the backend's default parallelism applies"
            );
        }

        // `top_k` has no `inferenceConfig` slot; it rides the model-specific
        // `additionalModelRequestFields`. The typed `top_k` is overlaid onto the raw object from
        // `extra`, as for `inferenceConfig`/`toolConfig`, re-emitting a same-protocol value and
        // carrying a cross-protocol one (e.g. from Anthropic) instead of dropping it. The raw object
        // is consumed here and skipped by the extra-merge; typed `top_k` wins.
        let mut additional_fields = req
            .extra
            .get(super::ADDITIONAL_MODEL_REQUEST_FIELDS)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(top_k) = req.top_k {
            // Re-emit `topK` when the reader stamped the sentinel (the body arrived as `topK`), else
            // `top_k`. The sentinel survives only same-protocol, so cross-protocol takes `top_k`.
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

        // The typed metadata is `requestMetadata`, same keys; a same-protocol body's own raw object
        // in `extra` (re-emitted below) wins.
        if let Some(pairs) = req
            .metadata
            .as_deref()
            .filter(|_| !req.extra.contains_key(super::FIELD_REQUEST_METADATA))
        {
            if let Some(m) = super::write_bedrock_request_metadata(pairs) {
                out.insert(super::FIELD_REQUEST_METADATA.to_string(), m);
            }
        }
        // The typed slots with no Converse form, as `dropped_egress_controls` reports.
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
            // `inferenceConfig` and `toolConfig` were consumed above; re-inserting the raw copy would
            // clobber the typed overlay. Every other unmodeled field passes through verbatim.
            if key == super::INFERENCE_CONFIG
                || key == keys::TOOL_CONFIG
                || key == super::OUTPUT_CONFIG_CAMEL
            {
                continue;
            }
            // `additionalModelRequestFields` was consumed above (typed `top_k` overlaid); the raw copy
            // would clobber it.
            if key == super::ADDITIONAL_MODEL_REQUEST_FIELDS {
                continue;
            }
            // The cachePoint stash is a busbar-internal sentinel, already spliced into
            // `system`/`messages`; emitting it would leak it onto the wire (an invalid body and a tell).
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
            // The top_k spelling hint is an internal sentinel, already consumed (it selected the key
            // in `additionalModelRequestFields`); it must not leak.
            if key == TOP_K_CAMEL_SENTINEL {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        serde_json::Value::Object(out)
    }
}
