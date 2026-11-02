use super::*;
use crate::codec::keys;

/// Converse `usage.totalTokens`: every token consumed, cache tokens included. AWS reports

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
        if stream {
            format!("/model/{}/converse-stream", model)
        } else {
            format!("/model/{}/converse", model)
        }
    }

    fn rewrite_model_if_needed(&self, _body: &mut serde_json::Value, _model: &str) -> bool {
        false
    }

    /// Converse frames a turn as `{role, content: [{text}]}`; a verbatim insert of `{role, content:

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
        let mut dropped = Vec::new();

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
            IrStreamEvent::MessageStart { .. } => {
                self.begin_stream();
                Some((
                    ET_MESSAGE_START.to_string(),
                    serde_json::json!({ (keys::ROLE): keys::ASSISTANT }),
                ))
            }

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => match block {
                crate::codec::ir::IrBlockMeta::Text => {
                    self.mark_block_open(*index);
                    None
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    let index = self.mark_block_open(*index);
                    Some((
                        ET_CONTENT_BLOCK_START.to_string(),
                        serde_json::json!({
                            (super::CONTENT_BLOCK_INDEX): index,
                            (keys::START): { (super::TOOL_USE_CAMEL): { (super::TOOL_USE_ID): id, (keys::NAME): name } }
                        }),
                    ))
                }

                crate::codec::ir::IrBlockMeta::Thinking { .. }
                | crate::codec::ir::IrBlockMeta::RedactedThinking => {
                    self.mark_block_open(*index);
                    None
                }
                crate::codec::ir::IrBlockMeta::Image => None,
            },

            IrStreamEvent::BlockDelta { index, delta } => {
                let index = self.wire_index(*index);
                match delta {
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

                    crate::codec::ir::IrDelta::ThinkingDelta(text) => Some((
                        ET_CONTENT_BLOCK_DELTA.to_string(),
                        serde_json::json!({
                            (super::CONTENT_BLOCK_INDEX): index,
                            (keys::DELTA): { (super::REASONING_CONTENT): { (keys::TEXT): text } }
                        }),
                    )),

                    crate::codec::ir::IrDelta::SignatureDelta(sig) => Some((
                        ET_CONTENT_BLOCK_DELTA.to_string(),
                        serde_json::json!({
                            (super::CONTENT_BLOCK_INDEX): index,
                            (keys::DELTA): { (super::REASONING_CONTENT): { (keys::SIGNATURE): sig } }
                        }),
                    )),

                    crate::codec::ir::IrDelta::MediaDelta(block) => {
                        crate::codec::drops::writer_drop!(
                        crate::codec::drops::block(block.kind_name()),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [], "dropping generated media on Bedrock streaming egress: no native media delta"
                    );
                        None
                    }
                    crate::codec::ir::IrDelta::RedactedReasoningDelta(redacted) => Some((
                        ET_CONTENT_BLOCK_DELTA.to_string(),
                        serde_json::json!({
                            (super::CONTENT_BLOCK_INDEX): index,
                            (keys::DELTA): { (super::REASONING_CONTENT): { (super::REDACTED_CONTENT): redacted } }
                        }),
                    )),

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

                    crate::codec::ir::IrDelta::LogprobsDelta(_) => None,
                }
            }

            IrStreamEvent::BlockStop { index } => {
                if self.take_block_open(*index) {
                    Some((
                        ET_CONTENT_BLOCK_STOP.to_string(),
                        serde_json::json!({ (super::CONTENT_BLOCK_INDEX): self.wire_index(*index) }),
                    ))
                } else {
                    None
                }
            }

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

            IrStreamEvent::MessageStop => {
                self.finish_stream();
                None
            }

            IrStreamEvent::Error(err) => {
                self.finish_stream();
                let (exception_name, message) = bedrock_stream_exception_for(err);
                Some((
                    exception_name.to_string(),
                    serde_json::json!({ (keys::MESSAGE): message }),
                ))
            }
        }
    }

    /// A `citation` delta carries one citation, so a multi-citation delta frames as several events at

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

    fn write_response_exception(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        err: &busbar_contract::protocol::IrError,
    ) -> Option<(String, String)> {
        self.finish_stream();
        let (exception_name, message) = bedrock_stream_exception_for(err);
        Some((exception_name.to_string(), message))
    }

    fn write_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        let mut content_arr: Vec<serde_json::Value> = Vec::new();
        if resp.audio.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("choices[].message.audio"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [], "dropping native Chat audio on Bedrock response egress: no native output-audio member"
            );
        }

        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            match block {
                crate::codec::ir::IrBlock::Text {
                    text, citations, ..
                } => {
                    if text.is_empty() {
                        continue;
                    }

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

                block @ crate::codec::ir::IrBlock::Media { .. } => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::block(block.kind_name()),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [], "dropping generated media on Bedrock response egress: no native attachment member"
                    );
                }

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
                    if let Some(image_block) = bedrock_image_block(source) {
                        content_arr.push(serde_json::json!({ (keys::IMAGE): image_block }));
                    }
                }
                block @ (crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. }) => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::block(block.kind_name()),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [], "dropping an answer block on Bedrock response egress: no native assistant member"
                    );
                }

                crate::codec::ir::IrBlock::Thinking {
                    text,
                    signature,
                    redacted,
                    ..
                } => {
                    content_arr.push(bedrock_reasoning_block(text, signature, *redacted));
                }

                crate::codec::ir::IrBlock::ToolResult { .. } => {}
            }
        }

        if content_arr.is_empty() {
            content_arr.push(serde_json::json!({ (keys::TEXT): "" }));
        }

        let reverse_reason = stop_reason_reverse_detailed(
            resp.stop_reason
                .unwrap_or(crate::codec::ir::IrStopReason::EndTurn),
            resp.stop_detail.as_ref(),
        );

        let mut usage_obj = serde_json::Map::new();
        usage_obj.insert(
            super::INPUT_TOKENS_CAMEL.to_string(),
            resp.usage.input_tokens.into(),
        );
        usage_obj.insert(
            super::OUTPUT_TOKENS_CAMEL.to_string(),
            resp.usage.output_tokens.into(),
        );

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
        attach_bedrock_error_headers(host, headers, kind);
    }

    fn new_stream_framing(&self) -> Box<dyn super::StreamFraming> {
        Box::<BedrockStreamFraming>::default()
    }

    fn wrap_buffered_as_stream(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ir: &crate::codec::ir::IrResponse,
        elapsed_ms: Option<u64>,
    ) -> Option<Vec<u8>> {
        Some(bedrock_response_to_eventstream(host, ir, elapsed_ms))
    }

    fn inject_response_metrics(&self, value: &mut serde_json::Value, elapsed_ms: Option<u64>) {
        super::ensure_metrics(value, elapsed_ms);
    }

    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        upstream_request_id
            .map(String::from)
            .or_else(|| synth_amzn_request_id(host))
            .map(|id| (HDR_AMZN_REQUEST_ID, id))
    }

    /// IR-18: a `reasoningText.signature` reads back as Claude's or the Bedrock model's own (told

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

    fn write_request_with(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();

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

        let doc_video = req
            .extra
            .get(DOC_VIDEO_SENTINEL)
            .and_then(|v| v.as_object());
        let message_doc_video = doc_video
            .and_then(|dv| dv.get(keys::MESSAGES))
            .and_then(|v| v.as_array());

        let emit_inline_system_cache = system_cache_points.is_none();
        if !req.system.is_empty() || system_cache_points.is_some() || system_guard_content.is_some()
        {
            let mut text_arr: Vec<serde_json::Value> = Vec::new();

            let guard_modelled = stashed_ir_indices([system_guard_content], None);
            for (sys_idx, block) in req.system.iter().enumerate() {
                let _block_origin =
                    crate::codec::drops::block_scope(req.block_origins.system.get(sys_idx));

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

                    if emit_inline_system_cache && cache_control.is_some() {
                        text_arr.push(bedrock_cache_point());
                    }
                }
            }

            let merged = merge_marker_entries(system_cache_points, system_guard_content);
            splice_cache_points(&mut text_arr, &merged);

            if !text_arr.is_empty() {
                out.insert(keys::SYSTEM.to_string(), serde_json::Value::Array(text_arr));
            }
        }

        let emit_inline_message_cache = message_cache_points.is_none();
        let mut msgs_arr: Vec<serde_json::Value> = Vec::new();
        for (msg_idx, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(msg_idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let role_str = match msg.role {
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,

                crate::codec::ir::IrRole::Tool => keys::USER,

                crate::codec::ir::IrRole::System => continue,
            };

            let mut content_arr: Vec<serde_json::Value> = Vec::new();

            let stashed_here =
                stashed_ir_indices([message_doc_video, message_guard_content], Some(msg_idx));
            for (block_idx, block) in msg.content.iter().enumerate() {
                let _block_origin =
                    crate::codec::drops::block_scope(_message_origins.get(block_idx));

                if stashed_here.contains(&block_idx) {
                    continue;
                }

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

                let written_before = content_arr.len();
                match block {
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
                        for (block_index, inner_block) in content.iter().enumerate() {
                            let _block_origin = crate::codec::drops::child_scope(block_index);

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
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): id, (keys::NAME): name, (keys::INPUT): input }
                                    }));
                                }
                                crate::codec::ir::IrBlock::ToolResult {
                                    tool_use_id,
                                    is_error,
                                    ..
                                } => {
                                    inner_content.push(serde_json::json!({
                                        (keys::JSON): { (super::TOOL_USE_ID): tool_use_id, "isError": is_error }
                                    }));
                                }

                                crate::codec::ir::IrBlock::Thinking { .. } => {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::THINKING,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [],
                                        "dropping non-representable Thinking block inside a Bedrock toolResult");
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
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
                }

                if emit_inline_message_cache
                    && block_cache_control.is_some()
                    && content_arr.len() > written_before
                {
                    content_arr.push(bedrock_cache_point());
                }
            }

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

            if let Some(prev_content) = msgs_arr
                .last_mut()
                .filter(|last| last.get(keys::ROLE).and_then(|r| r.as_str()) == Some(role_str))
                .and_then(|last| last.get_mut(keys::CONTENT))
                .and_then(|c| c.as_array_mut())
            {
                prev_content.append(&mut content_arr);
                continue;
            }

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

        if req.reasoning.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::REASONING,
                &crate::codec::diagnostics::IR_DROP_REASONING,
                [],
                "dropping cross-protocol reasoning/thinking ask on Bedrock egress: no Bedrock \
                 Converse mapping, as in 1.5.5"
            );
        }

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

        let mut output_config = req
            .extra
            .get(super::OUTPUT_CONFIG_CAMEL)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        if let Some(rf) = &req.response_format {
            match super::write_bedrock_text_format(rf) {
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

        let mut tool_config = req
            .extra
            .get(keys::TOOL_CONFIG)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

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

                if tool.cache_control.is_some() {
                    tools_arr.push(bedrock_cache_point());
                }
            }

            tool_config.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }

        tool_config.remove(super::TOOL_CHOICE_CAMEL);

        if tool_config.contains_key(keys::TOOLS) {
            if let Some(tc) = &req.tool_choice {
                match write_bedrock_tool_choice(tc) {
                    Some(v) => {
                        tool_config.insert(super::TOOL_CHOICE_CAMEL.to_string(), v);
                    }

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

        if tool_config.contains_key(keys::TOOLS) {
            out.insert(
                keys::TOOL_CONFIG.to_string(),
                serde_json::Value::Object(tool_config),
            );
        }

        if req.parallel_tool_calls.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::PARALLEL_TOOL_CALLS,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping parallel_tool_calls on Bedrock egress: Converse has no parallelism \
                 control, so the backend's default parallelism applies"
            );
        }

        let mut additional_fields = req
            .extra
            .get(super::ADDITIONAL_MODEL_REQUEST_FIELDS)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(top_k) = req.top_k {
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

        if let Some(pairs) = req
            .metadata
            .as_deref()
            .filter(|_| !req.extra.contains_key(super::FIELD_REQUEST_METADATA))
        {
            if let Some(m) = super::write_bedrock_request_metadata(pairs) {
                out.insert(super::FIELD_REQUEST_METADATA.to_string(), m);
            }
        }

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
            if key == super::INFERENCE_CONFIG
                || key == keys::TOOL_CONFIG
                || key == super::OUTPUT_CONFIG_CAMEL
            {
                continue;
            }

            if key == super::ADDITIONAL_MODEL_REQUEST_FIELDS {
                continue;
            }

            if key == CACHE_POINTS_SENTINEL {
                continue;
            }

            if key == GUARD_CONTENT_SENTINEL {
                continue;
            }

            if key == DOC_VIDEO_SENTINEL {
                continue;
            }

            if key == TOP_K_CAMEL_SENTINEL {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        serde_json::Value::Object(out)
    }
}
