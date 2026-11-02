use super::*;
use crate::codec::keys;

impl ProtocolWriter for AnthropicWriter {
    /// No image-fidelity slot: an image's `detail` is dropped, with the seam's warn.
    fn carries_image_detail(&self) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }

    fn upstream_path(&self) -> &str {
        PATH_UPSTREAM
    }

    /// IR-18: an Anthropic `signature` is read back as Claude's.
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        origin == crate::codec::ir::IrSignatureOrigin::Anthropic
    }

    /// Replace only the minted top-level `request_id`, and only where the writer put one, with the
    fn write_error_from_entropy(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
        entropy: &[u8],
    ) -> serde_json::Value {
        let mut envelope = self.write_error(host, status, kind, message);
        if let Some(obj) = envelope.as_object_mut() {
            if obj.contains_key(super::REQUEST_ID) {
                obj.insert(
                    super::REQUEST_ID.to_string(),
                    super::request_id_from_entropy(entropy)
                        .map_or(serde_json::Value::Null, serde_json::Value::String),
                );
            }
        }
        envelope
    }

    fn write_error(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        if status == STATUS_OVERLOADED || status == STATUS_ANTHROPIC_OVERLOADED {
            return Self::error_envelope(host, ERR_TYPE_OVERLOADED, message);
        }
        let anthropic_type = match kind {
            "invalid_request" | "bad_request" => ERR_TYPE_INVALID_REQUEST,
            "authentication" | keys::UNAUTHORIZED => ERR_TYPE_AUTHENTICATION,
            "permission" | "forbidden" => ERR_TYPE_PERMISSION,
            "not_found" => ERR_TYPE_NOT_FOUND,
            ERR_TYPE_REQUEST_TOO_LARGE | "payload_too_large" => ERR_TYPE_REQUEST_TOO_LARGE,
            "rate_limit" | "too_many_requests" => ERR_TYPE_RATE_LIMIT,

            busbar_contract::protocol::KIND_INSUFFICIENT_QUOTA | "quota_exceeded" => {
                ERR_TYPE_BILLING
            }

            crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH
            | busbar_contract::protocol::DISPOSITION_CONTEXT_LENGTH => ERR_TYPE_INVALID_REQUEST,
            busbar_contract::protocol::KIND_OVERLOADED => ERR_TYPE_OVERLOADED,
            crate::codec::framework::KIND_TIMEOUT => ERR_TYPE_TIMEOUT,
            ERR_TYPE_API_ERROR | crate::codec::framework::KIND_SERVER_ERROR | "internal" => {
                ERR_TYPE_API_ERROR
            }

            other => other,
        };
        Self::error_envelope(host, anthropic_type, message)
    }

    fn attach_error_response_headers(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        headers: &mut busbar_contract::http::HeaderMap,
        _kind: &str,
        envelope: &serde_json::Value,
    ) {
        if let Some(rid) = envelope.get(super::REQUEST_ID).and_then(|v| v.as_str()) {
            if let Ok(hv) = busbar_contract::http::HeaderValue::from_str(rid) {
                headers.insert(HDR_REQUEST_ID, hv);
            }
        }
    }

    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        upstream_request_id
            .map(String::from)
            .or_else(|| synth_anthropic_request_id(host))
            .map(|id| (HDR_REQUEST_ID, id))
    }

    fn reshape_for_path_base(&self, body: &mut serde_json::Value) -> bool {
        match body.as_object_mut() {
            Some(obj) => {
                obj.remove(keys::MODEL);
                obj.insert(
                    "anthropic_version".to_string(),
                    serde_json::json!("vertex-2023-10-16"),
                );
                true
            }
            None => false,
        }
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

        if req
            .response_format
            .as_ref()
            .is_some_and(|rf| rf.json && (!caps.native_structured_output || rf.schema.is_none()))
        {
            dropped.push(keys::RESPONSE_FORMAT);
        }
        dropped.extend(
            crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
                .map(crate::codec::carry::Slot::name),
        );

        dropped.extend(req.hosted_tools.iter().map(|h| h.kind_str()));
        dropped
    }

    fn write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        self.try_write_request_for_lane(host, req, model, caps)
            .unwrap_or_else(|error| {
                self.write_error(
                    host,
                    400,
                    ERR_TYPE_INVALID_REQUEST,
                    error
                        .provider_signal
                        .as_deref()
                        .unwrap_or(INVALID_TOOL_INPUT),
                )
            })
    }

    fn try_write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> Result<serde_json::Value, IrError> {
        validate_tool_inputs(&req.system)?;
        for message in &req.messages {
            validate_tool_inputs(&message.content)?;
        }
        let mut out = serde_json::Map::new();

        let mut system_blocks: Vec<serde_json::Value> = req
            .system
            .iter()
            .enumerate()
            .map(|(index, block)| {
                crate::codec::drops::with_block_origin(req.block_origins.system.get(index), || {
                    write_block(block)
                })
            })
            .collect();
        for (message_index, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(message_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            if msg.role == crate::codec::ir::IrRole::System {
                system_blocks.extend(msg.content.iter().enumerate().map(|(index, block)| {
                    crate::codec::drops::with_block_origin(_message_origins.get(index), || {
                        write_block(block)
                    })
                }));
            }
        }
        if !system_blocks.is_empty() {
            out.insert(
                keys::SYSTEM.to_string(),
                serde_json::Value::Array(system_blocks),
            );
        }

        let unmodeled_sentinel: &[serde_json::Value] = req
            .extra
            .get(ANTHROPIC_UNMODELED_BLOCKS_SENTINEL)
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let messages_array: Vec<_> = req
            .messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| msg.role != crate::codec::ir::IrRole::System)
            .map(|(m, msg)| {
                write_message(
                    msg,
                    m,
                    unmodeled_sentinel,
                    req.block_origins
                        .messages
                        .get(m)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                )
            })
            .collect();
        out.insert(
            keys::MESSAGES.to_string(),
            serde_json::Value::Array(messages_array),
        );

        crate::codec::dialect::warn_dropped(
            req.hosted_tools.iter().map(|h| h.kind_str()),
            &super::map::DROP_WARN,
        );
        let tools_array: Vec<_> = req
            .tools
            .iter()
            .filter(|t| {
                t.hosted.is_some()
                    || req
                        .allowed_tools
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&t.name))
            })
            .filter_map(write_tool)
            .collect();
        if !tools_array.is_empty() {
            out.insert(
                keys::TOOLS.to_string(),
                serde_json::Value::Array(tools_array),
            );
        }

        if let Some(tc) = &req.tool_choice {
            if !out.contains_key(keys::TOOLS) {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Anthropic egress: Anthropic rejects a tool_choice with \
                     no tools array (likely because the hosted tools that carried it were stripped \
                     on the cross-protocol seam)");
            } else {
                let mut tc_val = write_anthropic_tool_choice(tc);
                if let (Some(parallel), Some(map)) =
                    (req.parallel_tool_calls, tc_val.as_object_mut())
                {
                    if map.get(keys::TYPE).and_then(|t| t.as_str()) != Some(keys::NONE_WORD) {
                        map.insert(
                            super::DISABLE_PARALLEL_TOOL_USE.to_string(),
                            serde_json::json!(!parallel),
                        );
                    }
                }
                out.insert(keys::TOOL_CHOICE.to_string(), tc_val);
            }
        } else if let Some(parallel) = req.parallel_tool_calls {
            if out.contains_key(keys::TOOLS) {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    serde_json::json!({(keys::TYPE): keys::AUTO, (super::DISABLE_PARALLEL_TOOL_USE): !parallel}),
                );
            }
        }

        let mut output_config = serde_json::Map::new();

        if caps.native_structured_output {
            if let Some(rf) = req.response_format.as_ref().filter(|rf| rf.json) {
                match &rf.schema {
                    Some(schema) => {
                        let mut schema = schema.clone();
                        close_object_schemas(&mut schema);

                        if let (Some(desc), Some(obj)) = (&rf.description, schema.as_object_mut()) {
                            obj.entry(keys::DESCRIPTION)
                                .or_insert_with(|| serde_json::json!(desc));
                        }
                        output_config.insert(
                            keys::FORMAT.to_string(),
                            serde_json::json!({ (keys::TYPE): OUTPUT_FORMAT_JSON_SCHEMA, (keys::SCHEMA): schema }),
                        );
                    }
                    None => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::RESPONSE_FORMAT,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [parameter = keys::RESPONSE_FORMAT, ],
                            "dropping schema-less JSON mode on Anthropic egress: structured outputs \
                             require a JSON schema and the Messages API has no schema-less JSON mode \
                             (lossy-by-target)");
                    }
                }
            }
        } else if req.response_format.as_ref().is_some_and(|rf| rf.json) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::RESPONSE_FORMAT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [parameter = keys::RESPONSE_FORMAT,],
                "dropping response_format on Anthropic egress: this lane does not declare native \
                 structured outputs (lossy-by-target)"
            );
        }
        if let Some(max_tokens) = req.max_tokens {
            out.insert(keys::MAX_TOKENS.to_string(), serde_json::json!(max_tokens));
        }

        let mut thinking_emitted = false;
        match req.reasoning {
            Some(crate::codec::ir::IrReasoningAsk::Off) if caps.thinking_always_on => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "omitting reasoning OFF on Anthropic egress: this lane's model cannot switch \
                     thinking off (thinking_always_on) and rejects thinking.type \"disabled\""
                );
            }
            Some(crate::codec::ir::IrReasoningAsk::Off) => {
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_DISABLED }),
                );
            }
            Some(crate::codec::ir::IrReasoningAsk::Effort(effort))
                if caps.anthropic_adaptive_thinking =>
            {
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_ADAPTIVE }),
                );
                output_config.insert(
                    keys::EFFORT.to_string(),
                    serde_json::json!(anthropic_effort_word(effort)),
                );
                thinking_emitted = true;
            }
            Some(crate::codec::ir::IrReasoningAsk::Dynamic) if caps.anthropic_adaptive_thinking => {
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_ADAPTIVE }),
                );
                thinking_emitted = true;
            }

            Some(crate::codec::ir::IrReasoningAsk::Dynamic) => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "dropping a \"model decides\" reasoning ask on Anthropic egress: this lane does \
                     not declare adaptive thinking and budget_tokens has no dynamic form"
                );
            }

            Some(ask) => {
                let table = req
                    .reasoning_budgets
                    .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);

                if let Some(want) = ask.to_budget(table) {
                    let cap = req.max_tokens.map(|mt| mt.saturating_sub(1024));
                    let budget = cap.map_or(want, |c| want.min(c));
                    if budget >= 1024 {
                        if budget != want {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::REASONING,
                                &crate::codec::diagnostics::IR_DROP_REASONING,
                                [requested_budget = want, clamped_budget = budget, max_tokens = ?req.max_tokens, ],
                                "thinking budget clamped to fit under max_tokens");
                        }
                        out.insert(
                            keys::THINKING.to_string(),
                            serde_json::json!({(keys::TYPE): keys::ENABLED, (keys::BUDGET_TOKENS): budget}),
                        );
                        thinking_emitted = true;
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::REASONING,
                            &crate::codec::diagnostics::IR_DROP_REASONING,
                            [max_tokens = ?req.max_tokens, ],
                            "dropping reasoning ask on Anthropic egress: max_tokens leaves no room for \
                             the 1024-token thinking minimum");
                    }
                }
            }
            None => {}
        }
        if !output_config.is_empty() {
            out.insert(
                keys::OUTPUT_CONFIG.to_string(),
                serde_json::Value::Object(output_config),
            );
        }
        if thinking_emitted {
            if let Some(tc) = out
                .get_mut(keys::TOOL_CHOICE)
                .and_then(|v| v.as_object_mut())
            {
                let ty = tc.get(keys::TYPE).and_then(|t| t.as_str());
                if ty == Some(keys::ANY) || ty == Some(keys::TOOL) {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::TOOL_CHOICE,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [tool_choice = ?ty, ],
                        "downgrading forced/targeted tool_choice to 'auto' on Anthropic egress: \
                         not compatible with thinking");
                    tc.insert(keys::TYPE.to_string(), serde_json::json!(keys::AUTO));
                    tc.remove(keys::NAME);
                }
            }
        }

        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress {
                thinking: thinking_emitted,
            },
            &mut out,
        );
        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));

        crate::codec::carry::warn_drops(
            super::map::REQUEST,
            super::map::CONTROLS,
            Some(&super::map::DROP_WARN),
            req,
        );

        if let Some(user) = &req.user {
            out.insert(
                keys::METADATA.to_string(),
                serde_json::json!({(super::USER_ID): user}),
            );
        }
        for (key, value) in &req.extra {
            if key == ANTHROPIC_UNMODELED_BLOCKS_SENTINEL {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }
        Ok(serde_json::Value::Object(out))
    }

    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match self.try_write_response_events(host, ev) {
            Ok(frames) => frames.into_iter().next(),
            Err(_) => Some(("error".into(), self.encoding_failure(host))),
        }
    }

    fn try_write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, IrError> {
        let frame = match ev {
            IrStreamEvent::MessageStart {
                role,
                usage,
                id,
                model,
                ..
            } => {
                let role_str = match role {
                    crate::codec::ir::IrRole::User => keys::USER,
                    crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                    _ => return Ok(Vec::new()),
                };
                let mut msg_obj = serde_json::Map::new();

                let msg_id = self.carried_message_id(|| {
                    id.clone().map(Ok).unwrap_or_else(|| synth_message_id(host))
                })?;
                msg_obj.insert(keys::ID.to_string(), serde_json::json!(msg_id));
                msg_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::MESSAGE));
                msg_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));

                let model_str = model.as_deref().unwrap_or("");
                msg_obj.insert(keys::MODEL.to_string(), serde_json::json!(model_str));
                msg_obj.insert(
                    keys::CONTENT.to_string(),
                    serde_json::Value::Array(Vec::new()),
                );
                msg_obj.insert(super::STOP_REASON.to_string(), serde_json::Value::Null);
                msg_obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::Value::Null);

                msg_obj.insert(super::STOP_DETAILS.to_string(), serde_json::Value::Null);
                msg_obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);

                msg_obj.insert(keys::USAGE.to_string(), write_usage_object(usage.as_ref()));
                let mut data_obj = serde_json::Map::new();

                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(EVT_MESSAGE_START));
                data_obj.insert(
                    keys::MESSAGE.to_string(),
                    serde_json::Value::Object(msg_obj),
                );
                Some((
                    EVT_MESSAGE_START.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => {
                let content_block = match block {
                    IrBlockMeta::Text => {
                        serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "", (keys::CITATIONS): null })
                    }
                    IrBlockMeta::Thinking { .. } => {
                        serde_json::json!({ (keys::TYPE): keys::THINKING, (keys::THINKING): "", (keys::SIGNATURE): "" })
                    }

                    IrBlockMeta::RedactedThinking => return Ok(Vec::new()),
                    IrBlockMeta::ToolUse { id, name } => {
                        serde_json::json!({
                            (keys::TYPE): STOP_TOOL_USE,
                            (keys::ID): id,
                            (keys::NAME): name,
                            (keys::INPUT): {},
                            (super::W_CALLER): { (keys::TYPE): super::DIRECT },
                        })
                    }

                    IrBlockMeta::Image => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::IMAGE,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping image/attachment output block on Anthropic response egress: an Anthropic assistant message has no image or document response block"
                        );
                        return Ok(Vec::new());
                    }
                };
                if !self.mark_block_open(*index) {
                    return Ok(Vec::new());
                }
                if matches!(block, IrBlockMeta::ToolUse { .. }) {
                    self.stream.lock().tool_input_started.remove(index);
                }
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_START),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                data_obj.insert(super::CONTENT_BLOCK.to_string(), content_block);
                Some((
                    EVT_CONTENT_BLOCK_START.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::BlockDelta { index, delta } => {
                let delta_val = match delta {
                    IrDelta::TextDelta(text) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_TEXT, (keys::TEXT): text })
                    }
                    IrDelta::ThinkingDelta(thinking) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_THINKING, (keys::THINKING): thinking })
                    }
                    IrDelta::InputJsonDelta(json) => {
                        if let Some(first) = json
                            .bytes()
                            .find(|byte| !matches!(*byte, b' ' | b'\t' | b'\r' | b'\n'))
                        {
                            let mut stream = self.stream.lock();
                            if !stream.tool_input_started.contains(index) {
                                if first != b'{' {
                                    stream.invalid_tool_input = true;
                                    return Err(invalid_tool_input());
                                }
                                stream.tool_input_started.insert(*index);
                            }
                        }
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_INPUT_JSON, (super::PARTIAL_JSON): json })
                    }
                    IrDelta::SignatureDelta(sig) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_SIGNATURE, (keys::SIGNATURE): sig })
                    }

                    IrDelta::MediaDelta(_) => return Ok(Vec::new()),
                    IrDelta::RedactedReasoningDelta(bytes) => {
                        if !self.mark_block_open(*index) {
                            return Ok(Vec::new());
                        }
                        let mut data_obj = serde_json::Map::new();
                        data_obj.insert(
                            keys::TYPE.to_string(),
                            serde_json::json!(EVT_CONTENT_BLOCK_START),
                        );
                        data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                        data_obj.insert(
                            super::CONTENT_BLOCK.to_string(),
                            serde_json::json!({
                                (keys::TYPE): BLOCK_TYPE_REDACTED_THINKING,
                                (keys::DATA): bytes,
                            }),
                        );
                        return Ok(self.compact_frames(vec![(
                            EVT_CONTENT_BLOCK_START.to_string(),
                            serde_json::Value::Object(data_obj),
                        )]));
                    }

                    IrDelta::LogprobsDelta(_) => return Ok(Vec::new()),

                    IrDelta::CitationsDelta(citations) => {
                        let Some(c) = citations.first() else {
                            return Ok(Vec::new());
                        };
                        let Some(citation) = write_response_citation(c) else {
                            return Ok(Vec::new());
                        };
                        let mut data_obj = serde_json::Map::new();
                        data_obj.insert(
                            keys::TYPE.to_string(),
                            serde_json::json!(EVT_CONTENT_BLOCK_DELTA),
                        );
                        data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                        data_obj.insert(
                            keys::DELTA.to_string(),
                            serde_json::json!({
                                (keys::TYPE): DELTA_TYPE_CITATIONS,
                                (keys::CITATION): citation,
                            }),
                        );
                        return Ok(self.compact_frames(vec![(
                            EVT_CONTENT_BLOCK_DELTA.to_string(),
                            serde_json::Value::Object(data_obj),
                        )]));
                    }
                };
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_DELTA),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                data_obj.insert(keys::DELTA.to_string(), delta_val);
                Some((
                    EVT_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }

            IrStreamEvent::BlockStop { index } => {
                self.stream.lock().tool_input_started.remove(index);
                if !self.take_block_open(*index) {
                    return Ok(Vec::new());
                }
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_STOP),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                Some((
                    EVT_CONTENT_BLOCK_STOP.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::MessageDelta {
                stop_reason,
                stop_sequence,
                usage,
                stop_detail,
            } => {
                let mut delta_obj = serde_json::Map::new();
                if let Some(reason) = stop_reason {
                    delta_obj.insert(
                        super::STOP_REASON.to_string(),
                        serde_json::json!(write_anthropic_stop_reason_detailed(
                            *reason,
                            stop_detail.as_ref()
                        )),
                    );
                } else {
                    delta_obj.insert(super::STOP_REASON.to_string(), serde_json::Value::Null);
                }

                delta_obj.insert(
                    keys::STOP_SEQUENCE.to_string(),
                    stop_sequence
                        .as_deref()
                        .map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );

                delta_obj.insert(
                    super::STOP_DETAILS.to_string(),
                    write_anthropic_stop_details(*stop_reason, stop_detail.as_ref()),
                );
                delta_obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);

                let mut data_obj = serde_json::Map::new();
                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(EVT_MESSAGE_DELTA));
                data_obj.insert(
                    keys::DELTA.to_string(),
                    serde_json::Value::Object(delta_obj),
                );
                data_obj.insert(keys::USAGE.to_string(), write_message_delta_usage(usage));
                Some((
                    EVT_MESSAGE_DELTA.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::MessageStop => Some((
                EVT_MESSAGE_STOP.to_string(),
                serde_json::json!({ (keys::TYPE): EVT_MESSAGE_STOP }),
            )),
            IrStreamEvent::Error(err) => {
                let signal = err.provider_signal.as_deref().filter(|s| !s.is_empty());
                let (err_type, message) = match signal {
                    Some(ps) if is_error_type_token(ps) => (ps, ps.to_string()),
                    Some(ps) => (class_error_type(err.class), ps.to_string()),
                    None => (
                        class_error_type(err.class),
                        "an error occurred while streaming the response".to_string(),
                    ),
                };

                let mut error_obj = serde_json::Map::new();
                error_obj.insert(keys::TYPE.to_string(), serde_json::json!(err_type));
                error_obj.insert(keys::MESSAGE.to_string(), serde_json::json!(message));
                let mut data_obj = serde_json::Map::new();

                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::ERROR_WORD));
                data_obj.insert(
                    keys::ERROR_WORD.to_string(),
                    serde_json::Value::Object(error_obj),
                );

                data_obj.insert(
                    super::REQUEST_ID.to_string(),
                    serde_json::json!(synth_request_id(host).ok()),
                );
                Some((
                    keys::ERROR_WORD.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
        };
        Ok(self.compact_frames(frame.into_iter().collect()))
    }

    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        self.try_write_response(host, resp).unwrap_or_else(|error| {
            self.write_error(
                host,
                500,
                ERR_TYPE_API_ERROR,
                error
                    .provider_signal
                    .as_deref()
                    .unwrap_or("response encoding failed"),
            )
        })
    }

    fn try_write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> Result<serde_json::Value, IrError> {
        validate_tool_inputs(&resp.content)?;
        let mut obj = serde_json::Map::new();

        let id = resp
            .id
            .clone()
            .map(Ok)
            .unwrap_or_else(|| synth_message_id(host))?;
        obj.insert(keys::ID.to_string(), serde_json::json!(id));

        obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::MESSAGE));
        obj.insert(keys::ROLE.to_string(), serde_json::json!(keys::ASSISTANT));

        let model = resp.model.as_deref().unwrap_or("");
        obj.insert(keys::MODEL.to_string(), serde_json::json!(model));

        let content_array: Vec<serde_json::Value> = resp
            .content
            .iter()
            .enumerate()
            .filter_map(|(block_index, block)| {
                let _block_origin =
                    crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));
                match block {
                    crate::codec::ir::IrBlock::Image { .. }
                    | crate::codec::ir::IrBlock::Media { .. } => {
                        crate::codec::drops::writer_drop!(
                        crate::codec::drops::IMAGE,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "dropping image/attachment output block on Anthropic response egress: an \
                         Anthropic assistant message has no image or document response block"
                    );
                        None
                    }
                    crate::codec::ir::IrBlock::Json(v) => Some(serde_json::json!({
                        (keys::TYPE): keys::TEXT,
                        (keys::TEXT): serde_json::to_string(v).unwrap_or_default(),
                        (keys::CITATIONS): null,
                    })),
                    crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                        super::warn_hosted_record_dropped();
                        None
                    }
                    other => Some(write_response_block(other)),
                }
            })
            .collect();
        obj.insert(
            keys::CONTENT.to_string(),
            serde_json::Value::Array(content_array),
        );

        obj.insert(
            super::STOP_REASON.to_string(),
            resp.stop_reason
                .map(|reason| {
                    serde_json::json!(write_anthropic_stop_reason_detailed(
                        reason,
                        resp.stop_detail.as_ref()
                    ))
                })
                .unwrap_or(serde_json::Value::Null),
        );

        match &resp.stop_sequence {
            Some(seq) => {
                obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::json!(seq));
            }
            None => {
                obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::Value::Null);
            }
        }

        obj.insert(
            super::STOP_DETAILS.to_string(),
            write_anthropic_stop_details(resp.stop_reason, resp.stop_detail.as_ref()),
        );
        obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);

        obj.insert(
            keys::USAGE.to_string(),
            write_usage_object(Some(&resp.usage)),
        );

        Ok(serde_json::Value::Object(obj))
    }

    fn encoding_failure(&self, host: &dyn busbar_contract::codec::CodecHost) -> serde_json::Value {
        let message = if self.stream.lock().invalid_tool_input {
            INVALID_TOOL_INPUT
        } else {
            "identifier entropy unavailable"
        };
        self.write_error(host, 500, ERR_TYPE_API_ERROR, message)
    }
}

const INVALID_TOOL_INPUT: &str = "Anthropic tool input must be a JSON object";

fn invalid_tool_input() -> IrError {
    IrError {
        class: StatusClass::ServerError,
        provider_signal: Some(INVALID_TOOL_INPUT.into()),
        retry_after: None,
    }
}

fn validate_tool_inputs(blocks: &[crate::codec::ir::IrBlock]) -> Result<(), IrError> {
    for block in blocks {
        match block {
            crate::codec::ir::IrBlock::ToolUse { input, .. } if !input.is_object() => {
                return Err(invalid_tool_input())
            }
            crate::codec::ir::IrBlock::ToolResult { content, .. } => validate_tool_inputs(content)?,
            _ => {}
        }
    }
    Ok(())
}

/// The nine `error.type` tokens of Anthropic's published ErrorResponse.
fn is_error_type_token(s: &str) -> bool {
    [
        ERR_TYPE_API_ERROR,
        ERR_TYPE_AUTHENTICATION,
        ERR_TYPE_BILLING,
        ERR_TYPE_INVALID_REQUEST,
        ERR_TYPE_NOT_FOUND,
        ERR_TYPE_OVERLOADED,
        ERR_TYPE_PERMISSION,
        ERR_TYPE_RATE_LIMIT,
        ERR_TYPE_TIMEOUT,
    ]
    .contains(&s)
}

/// The Anthropic `error.type` for an error class, when the provider named no type of its own.
fn class_error_type(class: busbar_contract::upstream::StatusClass) -> &'static str {
    use busbar_contract::upstream::StatusClass as C;
    match class {
        C::RateLimit => ERR_TYPE_RATE_LIMIT,
        C::Overloaded => ERR_TYPE_OVERLOADED,
        C::Timeout => ERR_TYPE_TIMEOUT,
        C::Auth => ERR_TYPE_AUTHENTICATION,
        C::Billing => ERR_TYPE_BILLING,
        C::ClientError | C::ContextLength => ERR_TYPE_INVALID_REQUEST,
        C::ServerError | C::Network => ERR_TYPE_API_ERROR,
    }
}
