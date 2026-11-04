use super::*;

/// One Responses SSE frame: the event name, and its data, whose `type` IS the event name (spelled
/// once per frame here; `sequence_number` is stamped by `write_response_events`).
fn event(name: &'static str, mut data: serde_json::Value) -> (String, serde_json::Value) {
    if let Some(obj) = data.as_object_mut() {
        obj.insert(keys::TYPE.to_string(), serde_json::Value::from(name));
    }
    (name.to_string(), data)
}

impl ProtocolWriter for ResponsesWriter {
    fn upstream_path(&self) -> &str {
        "/v1/responses"
    }

    /// Latch the original ingress request body so `write_response_events` can answer the spec's
    /// request-echo members with the client's values (see `ResponsesWriter::store_request_echo`).
    fn set_request_echo(&self, ingress_request_body: &serde_json::Value) {
        self.store_request_echo(ingress_request_body);
    }

    /// Latch the stream's translation cap from the plane's settings.
    fn set_translate_cap(&self, cap: usize) {
        self.store_translate_cap(cap);
    }

    fn encoding_failure(&self, host: &dyn busbar_contract::codec::CodecHost) -> serde_json::Value {
        let message = if self.accumulation_failed.load(Ordering::Relaxed) {
            ACCUMULATION_FAILURE_MSG
        } else {
            "identifier entropy unavailable"
        };
        self.write_error(host, 500, ERR_TYPE_SERVER_ERROR, message)
    }

    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        let mut dropped: Vec<&'static str> =
            crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
                .map(crate::codec::carry::Slot::name)
                .collect();

        for tool in &req.hosted_tools {
            if matches!(tool, crate::codec::ir::IrHostedTool::WebFetch(_)) {
                dropped.push(tool.kind_str());
            }
        }

        if crate::codec::carry::Slot::OutputModalities.carried(req) {
            dropped.push("modalities");
        }
        dropped
    }

    /// `input` may be a list of items or a bare string (one implicit user turn); either is replaced
    /// by the EasyInputMessage list.
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> bool {
        if obj.get(keys::INPUT).is_none() {
            return false;
        }
        let Some(pairs) = crate::codec::dialect::rewrite_text_pairs(messages) else {
            return false;
        };
        let framed: Vec<serde_json::Value> = pairs
            .into_iter()
            .map(|(role, text)| serde_json::json!({ (keys::ROLE): role, (keys::CONTENT): text }))
            .collect();
        obj.insert(keys::INPUT.to_string(), serde_json::Value::Array(framed));
        true
    }

    fn write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        model: &str,
        caps: &crate::codec::egress_prep::LaneCaps,
    ) -> serde_json::Value {
        self.try_write_request_for_lane(host, req, model, caps)
            .unwrap_or_else(|_| self.encoding_failure(host))
    }

    fn try_write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &crate::codec::egress_prep::LaneCaps,
    ) -> Result<serde_json::Value, IrError> {
        let mut out = serde_json::Map::new();
        let mut input_arr: Vec<serde_json::Value> = Vec::new();

        if !req.system.is_empty() {
            let instructions: String = req
                .system
                .iter()
                .enumerate()
                .filter_map(|(block_index, block)| {
                    let _block_origin =
                        crate::codec::drops::block_scope(req.block_origins.system.get(block_index));
                    match block {
                        crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            if !instructions.is_empty() {
                if req.system_role == Some(crate::codec::ir::IrSystemRole::Developer) {
                    input_arr.push(serde_json::json!({
                        (keys::ROLE): crate::codec::ir::IrSystemRole::Developer.as_str(),
                        (keys::CONTENT): instructions
                    }));
                } else {
                    out.insert(
                        keys::INSTRUCTIONS.to_string(),
                        serde_json::json!(instructions),
                    );
                }
            }
        }

        for (message_index, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(message_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            match msg.role {
                crate::codec::ir::IrRole::User | crate::codec::ir::IrRole::Assistant => {
                    let role_str = if msg.role == crate::codec::ir::IrRole::User {
                        keys::USER
                    } else {
                        keys::ASSISTANT
                    };

                    let mut content_arr: Vec<serde_json::Value> = Vec::new();

                    let flush_content =
                        |content: &mut Vec<serde_json::Value>,
                         input: &mut Vec<serde_json::Value>| {
                            if !content.is_empty() {
                                input.push(serde_json::json!({
                                    (keys::ROLE): role_str,
                                    (keys::CONTENT): std::mem::take(content),
                                }));
                            }
                        };
                    for (block_index, block) in msg.content.iter().enumerate() {
                        let _block_origin =
                            crate::codec::drops::block_scope(_message_origins.get(block_index));

                        match block {
                            crate::codec::ir::IrBlock::Text {
                                text,
                                refusal: true,
                                ..
                            } if msg.role == crate::codec::ir::IrRole::Assistant => {
                                content_arr.push(serde_json::json!({
                                    (keys::TYPE): keys::REFUSAL,
                                    (keys::REFUSAL): text
                                }));
                            }
                            crate::codec::ir::IrBlock::Text {
                                text, citations, ..
                            } => {
                                let type_str = if msg.role == crate::codec::ir::IrRole::User {
                                    CONTENT_TYPE_INPUT_TEXT
                                } else {
                                    CONTENT_TYPE_OUTPUT_TEXT
                                };
                                let mut part = serde_json::Map::new();
                                part.insert(keys::TYPE.to_string(), serde_json::json!(type_str));
                                part.insert(keys::TEXT.to_string(), serde_json::json!(text));

                                if msg.role == crate::codec::ir::IrRole::Assistant
                                    && !citations.is_empty()
                                {
                                    let annotations =
                                        super::super::url_citation_wire::url_annotations(
                                            text, 0, citations,
                                        );
                                    if !annotations.is_empty() {
                                        part.insert(
                                            keys::ANNOTATIONS.to_string(),
                                            serde_json::Value::Array(annotations),
                                        );
                                    }
                                }
                                content_arr.push(serde_json::Value::Object(part));
                            }
                            crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                                content_arr.extend(input_image_part(source, *detail));
                            }

                            crate::codec::ir::IrBlock::Media {
                                kind, source, name, ..
                            } => {
                                content_arr.extend(input_file_part(*kind, source, name.as_deref()));
                            }
                            crate::codec::ir::IrBlock::Json(_)
                            | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
                            crate::codec::ir::IrBlock::ToolUse {
                                id, name, input, ..
                            } => {
                                let args_str =
                                    crate::codec::dialect::tool_arguments_to_string(input);
                                flush_content(&mut content_arr, &mut input_arr);
                                input_arr.push(serde_json::json!({
                                    (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                                    (CALL_ID): id,
                                    (keys::NAME): name,
                                    (keys::ARGUMENTS): args_str
                                }));
                            }
                            crate::codec::ir::IrBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                let output_value = function_call_output_value(content);

                                flush_content(&mut content_arr, &mut input_arr);
                                input_arr.push(serde_json::json!({
                                    (keys::TYPE): FUNCTION_CALL_OUTPUT,
                                    (CALL_ID): tool_use_id,
                                    (keys::OUTPUT): output_value
                                }));
                            }

                            crate::codec::ir::IrBlock::Thinking {
                                text,
                                signature,
                                redacted,
                                kind,
                                signature_origin,
                                ..
                            } if !*redacted && msg.role == crate::codec::ir::IrRole::Assistant => {
                                let emit_sig = signature
                                    .as_deref()
                                    .filter(|_| super::slots::own_signature(*signature_origin));
                                if signature.is_some() && emit_sig.is_none() {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::THINKING,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [origin = ?signature_origin, ],
                                        "dropping a foreign reasoning signature on Responses egress: \
                                         `encrypted_content` accepts only an OpenAI-minted blob");
                                }

                                if !text.is_empty() || emit_sig.is_some() {
                                    let mut item = serde_json::Map::new();
                                    item.insert(
                                        keys::TYPE.to_string(),
                                        serde_json::json!(ITEM_TYPE_REASONING),
                                    );
                                    item.insert(
                                        keys::ID.to_string(),
                                        serde_json::json!(synthesize_item_id(
                                            host,
                                            ITEM_ID_PREFIX_RS
                                        )?),
                                    );

                                    super::slots::insert_reasoning_text(&mut item, text, *kind);
                                    if let Some(sig) = emit_sig {
                                        item.insert(
                                            ENCRYPTED_CONTENT.to_string(),
                                            serde_json::json!(sig),
                                        );
                                    }
                                    flush_content(&mut content_arr, &mut input_arr);
                                    input_arr.push(serde_json::Value::Object(item));
                                }
                            }

                            crate::codec::ir::IrBlock::Thinking { redacted, .. } if !*redacted => {
                                crate::codec::drops::writer_drop!(
                                    crate::codec::drops::THINKING,
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [],
                                    "dropping non-Assistant Thinking block on Responses egress: a \
                                     `reasoning` input item asserts it is the model's own prior \
                                     reasoning, which only an Assistant-role message can carry"
                                );
                            }

                            crate::codec::ir::IrBlock::Thinking { .. } => {}
                        }
                    }

                    flush_content(&mut content_arr, &mut input_arr);
                }

                crate::codec::ir::IrRole::Tool => {
                    for (block_index, block) in msg.content.iter().enumerate() {
                        let _block_origin =
                            crate::codec::drops::block_scope(_message_origins.get(block_index));

                        if let crate::codec::ir::IrBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = block
                        {
                            let output_value = function_call_output_value(content);

                            input_arr.push(serde_json::json!({
                                (keys::TYPE): FUNCTION_CALL_OUTPUT,
                                (CALL_ID): tool_use_id,
                                (keys::OUTPUT): output_value
                            }));
                        }
                    }
                }

                crate::codec::ir::IrRole::System => {}
            }
        }

        if !input_arr.is_empty() {
            out.insert(keys::INPUT.to_string(), serde_json::Value::Array(input_arr));
        }

        let hosted_arr: Vec<serde_json::Value> = req
            .hosted_tools
            .iter()
            .filter_map(super::slots::write_hosted_tool)
            .collect();
        let has_tools = !req.tools.is_empty() || !hosted_arr.is_empty();
        if has_tools {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in &req.tools {
                if let Some(hosted) = &tool.hosted {
                    tools_arr.push(hosted.clone());
                    continue;
                }
                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::FUNCTION));
                tool_obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));

                if let Some(desc) = &tool.description {
                    tool_obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }

                let params = if !tool.input_schema.is_null() {
                    tool.input_schema.clone()
                } else {
                    serde_json::json!({})
                };
                tool_obj.insert(keys::PARAMETERS.to_string(), params);

                if let Some(strict) = tool.strict {
                    tool_obj.insert(keys::STRICT.to_string(), serde_json::json!(strict));
                }

                tools_arr.push(serde_json::Value::Object(tool_obj));
            }
            tools_arr.extend(hosted_arr);
            out.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }

        if let Some(names) = &req.allowed_tools {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping allowed_tools tool_choice on Responses egress: tool_choice is only \
                     allowed when tools are specified"
                );
            } else {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    super::slots::write_allowed_tools(names, req.tool_choice.as_ref()),
                );
            }
        } else if let Some(tc) = &req.tool_choice {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Responses egress: tool_choice is only allowed when \
                     tools are specified (likely because the hosted tools that carried it were \
                     stripped on the cross-protocol seam)"
                );
            } else {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    write_responses_tool_choice(tc),
                );
            }
        }

        if let Some(parallel) = req.parallel_tool_calls {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::PARALLEL_TOOL_CALLS,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping parallel_tool_calls on Responses egress: it has no accompanying \
                     tools (likely because the hosted tools that carried it were stripped on the \
                     cross-protocol seam), so the backend's default parallelism applies"
                );
            } else {
                out.insert(
                    keys::PARALLEL_TOOL_CALLS.to_string(),
                    serde_json::json!(parallel),
                );
            }
        }

        if let Some(max_tokens) = req.max_tokens {
            out.insert(
                INCOMPLETE_REASON_MAX_OUTPUT.to_string(),
                serde_json::json!(max_tokens),
            );
        }

        if let Some(top_logprobs) = req.top_logprobs {
            out.insert(
                keys::TOP_LOGPROBS.to_string(),
                serde_json::json!(top_logprobs),
            );
        }

        if let Some(user) = &req.user {
            out.insert(keys::USER.to_string(), serde_json::json!(user));
        }

        if crate::codec::carry::Slot::OutputModalities.carried(req) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::OUTPUT_MODALITIES,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "responses writer: /v1/responses models no output-modality ask; dropping the \
                 non-text modalities (lossy-by-target)"
            );
        }

        crate::codec::carry::warn_drops(super::map::REQUEST, super::map::CONTROLS, None, req);

        if let Some(rf) = &req.response_format {
            let format = write_text_format(rf);

            let mut text_obj = req
                .extra
                .get(keys::TEXT)
                .and_then(|t| t.as_object())
                .cloned()
                .unwrap_or_default();
            text_obj.insert(keys::FORMAT.to_string(), format);
            out.insert(keys::TEXT.to_string(), serde_json::Value::Object(text_obj));
        }

        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );

        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));

        if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) {
            if !req.extra.contains_key(keys::REASONING) {
                if caps.reasoning_none {
                    out.insert(
                        keys::REASONING.to_string(),
                        serde_json::json!({(keys::EFFORT): keys::NONE_WORD}),
                    );
                } else {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [],
                        "omitting reasoning OFF on Responses egress: reasoning.effort \"none\" is \
                         not accepted by every OpenAI reasoning model and this lane does not \
                         declare it"
                    );
                }
            }
        } else if let Some(ask) = req.reasoning {
            if !req.extra.contains_key(keys::REASONING) {
                let table = req
                    .reasoning_budgets
                    .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
                match ask.to_effort(table) {
                    Some(effort) => {
                        out.insert(
                            keys::REASONING.to_string(),
                            serde_json::json!({(keys::EFFORT): effort.as_three_word_str()}),
                        );
                    }

                    None => crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [],
                        "dropping a \"model decides\" reasoning ask on Responses egress: \
                         reasoning.effort has no dynamic form"
                    ),
                }
            }
        }

        for (key, value) in &req.extra {
            if key == keys::TEXT && req.response_format.is_some() {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        if req.logprobs == Some(true) {
            let include = out
                .entry(INCLUDE.to_string())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let Some(arr) = include.as_array_mut() {
                if !arr
                    .iter()
                    .any(|v| v.as_str() == Some(INCLUDE_OUTPUT_TEXT_LOGPROBS))
                {
                    arr.push(serde_json::json!(INCLUDE_OUTPUT_TEXT_LOGPROBS));
                }
            }
        }

        Ok(serde_json::Value::Object(out))
    }

    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        self.write_response_events(host, ev).into_iter().next()
    }

    fn write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        self.try_write_response_events(host, ev)
            .unwrap_or_else(|_| vec![("error".into(), self.encoding_failure(host))])
    }

    fn try_write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, IrError> {
        let opening = matches!(ev, IrStreamEvent::MessageStart { .. })
            && !self.started.swap(true, Ordering::Relaxed);
        if opening {
            self.reset_sequence_number();
        }
        if self.accumulation_failed.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }

        self.reserve_stream_event(ev)?;

        let frames: Vec<(String, serde_json::Value)> = match ev {
            IrStreamEvent::MessageStart {
                id,
                created,
                model,
                usage,
                ..
            } => {
                let mut resp_obj = serde_json::Map::new();

                let id = self
                    .carried_response_id()
                    .filter(|_| !opening)
                    .or_else(|| id.clone())
                    .map(Ok)
                    .unwrap_or_else(|| synthesize_response_id(host))?;

                self.set_response_id(&id);
                let created_at = created.unwrap_or_else(|| self.default_created_at(host));

                self.set_created_at(created_at);
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
                resp_obj.insert(keys::CREATED_AT.to_string(), serde_json::json!(created_at));
                resp_obj.insert(
                    keys::STATUS.to_string(),
                    serde_json::json!(STATUS_IN_PROGRESS),
                );

                let model_name = model.as_deref().unwrap_or(DEFAULT_MODEL);

                self.set_model(model_name);
                resp_obj.insert(keys::MODEL.to_string(), serde_json::json!(model_name));

                resp_obj.insert(keys::OUTPUT.to_string(), serde_json::json!([]));
                resp_obj.insert(keys::ERROR_WORD.to_string(), serde_json::Value::Null);
                resp_obj.insert(
                    keys::USAGE.to_string(),
                    usage
                        .as_ref()
                        .map(build_responses_usage)
                        .unwrap_or_else(zero_responses_usage),
                );

                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());
                vec![event(
                    EVT_RESPONSE_CREATED,
                    serde_json::json!({ (keys::RESPONSE): resp_obj }),
                )]
            }

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal,
            } => match block {
                crate::codec::ir::IrBlockMeta::Text => {
                    if !self.open_text_item(*index) {
                        return Ok(Vec::new());
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index)?;

                    if *refusal {
                        self.mark_refusal(*index);
                    }
                    let part = if *refusal {
                        serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): "" })
                    } else {
                        serde_json::json!({
                            (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                            (keys::TEXT): "",
                            (keys::ANNOTATIONS): [],
                            (keys::LOGPROBS): []
                        })
                    };
                    vec![
                        event(
                            EVT_OUTPUT_ITEM_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): {
                                    (keys::TYPE): ITEM_TYPE_MESSAGE,
                                    (keys::ID): item_id,
                                    (keys::ROLE): keys::ASSISTANT,
                                    (keys::STATUS): STATUS_IN_PROGRESS,
                                    (keys::CONTENT): []
                                }
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                // Every frame of the item uses the single part's `content_index: 0`.
                                (CONTENT_INDEX): 0,
                                // Opens an empty `output_text` part (text arrives via deltas), in the
                                // closing part's shape: `type`/`text`/`annotations`/`logprobs`, all required.
                                (PART): part
                            }),
                        ),
                    ]
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    if !self.mark_tool_open(*index) {
                        return Ok(Vec::new());
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_FC, *index)?;

                    self.record_tool_meta(*index, id, name);
                    vec![event(
                        EVT_OUTPUT_ITEM_ADDED,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): item_id,
                            // `arguments` is required on `FunctionToolCall`. Real OpenAI opens the
                            // item with `"arguments": ""` and fills it via the delta events; omitting
                            // it fails the schema and makes an SDK concatenate onto `undefined`.
                            (ITEM): {
                                (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                                (keys::ID): item_id,
                                (CALL_ID): id,
                                (keys::NAME): name,
                                (keys::ARGUMENTS): ""
                            }
                        }),
                    )]
                }

                crate::codec::ir::IrBlockMeta::RedactedThinking => Vec::new(),
                crate::codec::ir::IrBlockMeta::Thinking { kind } => {
                    if !self.open_reasoning_item(*index) {
                        return Ok(Vec::new());
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_RS, *index)?;

                    if *kind == Some(crate::codec::ir::IrThinkingKind::Summary) {
                        self.mark_summary_reasoning(*index);
                        vec![
                            event(
                                EVT_OUTPUT_ITEM_ADDED,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): {
                                        (keys::TYPE): ITEM_TYPE_REASONING,
                                        (keys::ID): item_id,
                                        (SUMMARY): []
                                    }
                                }),
                            ),
                            event(
                                EVT_REASONING_SUMMARY_PART_ADDED,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (PART): { (keys::TYPE): SUMMARY_TEXT, (keys::TEXT): "" }
                                }),
                            ),
                        ]
                    } else {
                        vec![event(
                            EVT_OUTPUT_ITEM_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): {
                                    (keys::TYPE): ITEM_TYPE_REASONING,
                                    (keys::ID): item_id,
                                    (SUMMARY): [],
                                    (keys::CONTENT): []
                                }
                            }),
                        )]
                    }
                }
                crate::codec::ir::IrBlockMeta::Image => Vec::new(),
            },

            IrStreamEvent::BlockDelta { index, delta } => match delta {
                crate::codec::ir::IrDelta::TextDelta(text)
                    if !text.is_empty() && self.is_refusal(*index) =>
                {
                    self.require_accumulation(self.append_text(*index, text))?;
                    vec![event(
                        EVT_REFUSAL_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index)?,
                            (CONTENT_INDEX): 0,
                            (keys::DELTA): text
                        }),
                    )]
                }
                crate::codec::ir::IrDelta::TextDelta(text) if !text.is_empty() => {
                    self.require_accumulation(self.append_text(*index, text))?;

                    vec![event(
                        EVT_OUTPUT_TEXT_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index)?,
                            (CONTENT_INDEX): 0,
                            (keys::DELTA): text,
                            (keys::LOGPROBS): []
                        }),
                    )]
                }
                crate::codec::ir::IrDelta::InputJsonDelta(json_str) => {
                    self.require_accumulation(self.append_tool_arguments(*index, json_str))?;
                    vec![event(
                        EVT_FUNCTION_CALL_ARGS_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_FC, *index)?,
                            (keys::DELTA): json_str
                        }),
                    )]
                }
                &crate::codec::ir::IrDelta::TextDelta(_) => Vec::new(),
                crate::codec::ir::IrDelta::ThinkingDelta(text) if !text.is_empty() => {
                    self.require_accumulation(self.append_reasoning(*index, text))?;
                    let (event, part_key) = if self.is_summary_reasoning(*index) {
                        (EVT_REASONING_SUMMARY_TEXT_DELTA, SUMMARY_INDEX)
                    } else {
                        (EVT_REASONING_TEXT_DELTA, CONTENT_INDEX)
                    };
                    vec![(
                        event.to_string(),
                        serde_json::json!({
                            (keys::TYPE): event,
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_RS, *index)?,
                            part_key: 0,
                            (keys::DELTA): text
                        }),
                    )]
                }

                crate::codec::ir::IrDelta::SignatureDelta(sig) => {
                    self.require_accumulation(self.append_reasoning_signature(*index, sig))?;
                    Vec::new()
                }

                &crate::codec::ir::IrDelta::ThinkingDelta(_)
                | crate::codec::ir::IrDelta::RedactedReasoningDelta(_) => Vec::new(),

                crate::codec::ir::IrDelta::MediaDelta(block) => {
                    drop_response_block(block);
                    Vec::new()
                }

                crate::codec::ir::IrDelta::CitationsDelta(cits) => {
                    self.require_accumulation(self.append_citations(*index, cits))?;
                    Vec::new()
                }

                crate::codec::ir::IrDelta::LogprobsDelta(lps) => {
                    self.require_accumulation(self.append_logprobs(*index, lps))?;
                    Vec::new()
                }
            },

            IrStreamEvent::BlockStop { index } => {
                if self.take_reasoning_open(*index) {
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_RS, *index)?;
                    let text = self.take_reasoning_accum(*index);
                    let summary = self.take_summary_reasoning(*index);

                    let mut item_obj = serde_json::Map::new();
                    item_obj.insert(
                        keys::TYPE.to_string(),
                        serde_json::json!(ITEM_TYPE_REASONING),
                    );
                    item_obj.insert(keys::ID.to_string(), serde_json::json!(item_id));
                    super::slots::insert_reasoning_text(
                        &mut item_obj,
                        &text,
                        summary.then_some(crate::codec::ir::IrThinkingKind::Summary),
                    );
                    let mut item = serde_json::Value::Object(item_obj);

                    if let Some(sig) = self.take_reasoning_signature(*index) {
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert(ENCRYPTED_CONTENT.to_string(), serde_json::json!(sig));
                        }
                    }
                    self.record_output_item(*index, &item)?;
                    if summary {
                        vec![
                            event(
                                EVT_REASONING_SUMMARY_TEXT_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (keys::TEXT): text,
                                }),
                            ),
                            event(
                                EVT_REASONING_SUMMARY_PART_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (PART): { (keys::TYPE): SUMMARY_TEXT, (keys::TEXT): text },
                                }),
                            ),
                            event(
                                EVT_OUTPUT_ITEM_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): item,
                                }),
                            ),
                        ]
                    } else {
                        vec![
                            event(
                                EVT_REASONING_TEXT_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (CONTENT_INDEX): 0,
                                    (keys::TEXT): text,
                                }),
                            ),
                            event(
                                EVT_OUTPUT_ITEM_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): item,
                                }),
                            ),
                        ]
                    }
                } else if self.take_tool_open(*index) {
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_FC, *index)?;
                    let accum = self.take_tool_accum(*index).unwrap_or_default();
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                        (keys::ID): item_id,
                        (CALL_ID): accum.call_id,
                        (keys::NAME): accum.name,
                        (keys::ARGUMENTS): accum.arguments,
                    });

                    self.record_output_item(*index, &item)?;
                    vec![
                        event(
                            EVT_FUNCTION_CALL_ARGS_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (keys::ARGUMENTS): accum.arguments,
                            }),
                        ),
                        event(
                            EVT_OUTPUT_ITEM_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): item,
                            }),
                        ),
                    ]
                } else if self.is_refusal(*index) && self.take_text_open(*index) {
                    self.take_refusal(*index);
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index)?;
                    let text = self.take_text_accum(*index);
                    let _ = self.take_citation_accum(*index);
                    let _ = self.take_logprob_accum(*index);
                    let part =
                        serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text });
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): item_id,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [part.clone()]
                    });
                    self.record_output_item(*index, &item)?;
                    vec![
                        event(
                            EVT_REFUSAL_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (keys::REFUSAL): text,
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (PART): part,
                            }),
                        ),
                        event(
                            EVT_OUTPUT_ITEM_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): item,
                            }),
                        ),
                    ]
                } else if self.take_text_open(*index) {
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index)?;
                    let text = self.take_text_accum(*index);
                    let citations = self.take_citation_accum(*index);
                    let mut annotations =
                        super::super::url_citation_wire::url_annotations(&text, 0, &citations);
                    annotations.extend(super::file_annotations(&citations));

                    let logprobs = self.take_logprob_accum(*index);
                    let part_logprobs = write_responses_part_logprobs(&logprobs);
                    let event_logprobs = write_responses_event_logprobs(&logprobs);
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): item_id,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [
                            {
                                (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                                (keys::TEXT): text,
                                (keys::ANNOTATIONS): annotations,
                                (keys::LOGPROBS): part_logprobs,
                            }
                        ]
                    });

                    self.record_output_item(*index, &item)?;
                    vec![
                        event(
                            EVT_OUTPUT_TEXT_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (keys::TEXT): text,
                                (keys::LOGPROBS): event_logprobs,
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                // The finalized part, same shape as the message item's content part.
                                (PART): {
                                    (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                                    (keys::TEXT): text,
                                    (keys::ANNOTATIONS): annotations,
                                    (keys::LOGPROBS): part_logprobs,
                                }
                            }),
                        ),
                        event(
                            EVT_OUTPUT_ITEM_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): item,
                            }),
                        ),
                    ]
                } else {
                    Vec::new()
                }
            }

            IrStreamEvent::MessageDelta {
                stop_reason,
                usage,
                stop_sequence: _,
                stop_detail: _,
            } => {
                if self.failed.load(Ordering::Relaxed) {
                    return Ok(Vec::new());
                }

                let generation_failed = *stop_reason == Some(crate::codec::ir::IrStopReason::Error);
                let status = if generation_failed {
                    STATUS_FAILED
                } else {
                    stop_reason
                        .map(write_responses_status)
                        .unwrap_or(STATUS_COMPLETED)
                };

                let mut resp_obj = serde_json::Map::new();

                let response_id = self
                    .carried_response_id()
                    .map(Ok)
                    .unwrap_or_else(|| synthesize_response_id(host))?;
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(response_id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));

                resp_obj.insert(
                    keys::CREATED_AT.to_string(),
                    serde_json::json!(self.carried_created_at(host)),
                );
                resp_obj.insert(keys::STATUS.to_string(), serde_json::json!(status));

                resp_obj.insert(
                    keys::MODEL.to_string(),
                    serde_json::json!(self.carried_model()),
                );

                if status == STATUS_INCOMPLETE {
                    let reason = stop_reason
                        .map(write_responses_incomplete_reason)
                        .unwrap_or(INCOMPLETE_REASON_OTHER);
                    let mut incomplete_details = serde_json::Map::new();
                    incomplete_details.insert(keys::REASON.to_string(), serde_json::json!(reason));
                    resp_obj.insert(
                        INCOMPLETE_DETAILS.to_string(),
                        serde_json::Value::Object(incomplete_details),
                    );
                }

                resp_obj.insert(keys::USAGE.to_string(), build_responses_usage(usage));

                resp_obj.insert(
                    keys::OUTPUT.to_string(),
                    serde_json::Value::Array(self.drain_output_items()),
                );
                if generation_failed {
                    self.failed.store(true, Ordering::Relaxed);
                    resp_obj.insert(
                        keys::ERROR_WORD.to_string(),
                        serde_json::json!({ (keys::CODE): ERR_TYPE_SERVER_ERROR, (keys::MESSAGE): keys::ERROR_WORD }),
                    );
                } else {
                    resp_obj.insert(keys::ERROR_WORD.to_string(), serde_json::Value::Null);
                }

                if let Some(tier) =
                    usage.detail.service_tier.as_deref().and_then(|t| {
                        crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t)
                    })
                {
                    resp_obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
                }

                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());

                let event_name = match status {
                    STATUS_FAILED => EVT_RESPONSE_FAILED,
                    STATUS_INCOMPLETE => EVT_RESPONSE_INCOMPLETE,
                    _ => EVT_RESPONSE_COMPLETED,
                };
                vec![(
                    event_name.to_string(),
                    serde_json::json!({ (keys::TYPE): event_name, (keys::RESPONSE): resp_obj }),
                )]
            }

            IrStreamEvent::MessageStop => Vec::new(),

            IrStreamEvent::Error(err) => {
                let message = err
                    .provider_signal
                    .clone()
                    .unwrap_or_else(|| keys::ERROR_WORD.to_string());

                let code = responses_error_code(err);

                let response_id = self
                    .carried_response_id()
                    .map(Ok)
                    .unwrap_or_else(|| synthesize_response_id(host))?;
                let mut resp_obj = serde_json::Map::new();
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(response_id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));

                resp_obj.insert(
                    keys::CREATED_AT.to_string(),
                    serde_json::json!(self.carried_created_at(host)),
                );

                resp_obj.insert(
                    keys::MODEL.to_string(),
                    serde_json::json!(self.carried_model()),
                );
                resp_obj.insert(keys::STATUS.to_string(), serde_json::json!(STATUS_FAILED));

                resp_obj.insert(keys::OUTPUT.to_string(), serde_json::json!([]));
                resp_obj.insert(
                    keys::ERROR_WORD.to_string(),
                    serde_json::json!({ (keys::CODE): code, (keys::MESSAGE): message }),
                );
                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());

                self.failed.store(true, Ordering::Relaxed);
                vec![event(
                    EVT_RESPONSE_FAILED,
                    serde_json::json!({ (keys::RESPONSE): resp_obj }),
                )]
            }
        };

        Ok(frames
            .into_iter()
            .map(|(event_name, mut data)| {
                self.compact_output_index(&event_name, &mut data);
                if let Some(obj) = data.as_object_mut() {
                    obj.insert(
                        "sequence_number".to_string(),
                        serde_json::json!(self.next_sequence_number()),
                    );
                }
                (event_name, data)
            })
            .collect())
    }

    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        self.try_write_response(host, resp)
            .unwrap_or_else(|_| self.encoding_failure(host))
    }

    fn try_write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> Result<serde_json::Value, IrError> {
        if resp.audio.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("choices[].message.audio"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping native Chat audio on Responses egress: no native output-audio item"
            );
        }

        let status = resp
            .stop_reason
            .map(write_responses_status)
            .unwrap_or(STATUS_COMPLETED);

        let mut output_arr: Vec<serde_json::Value> = Vec::new();

        let mut pending_logprobs: Option<&[crate::codec::ir::IrTokenLogprob]> =
            Some(&resp.logprobs);
        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            match block {
                crate::codec::ir::IrBlock::Text {
                    text,
                    refusal: true,
                    ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_MSG)?,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [{ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text }]
                    }));
                }
                crate::codec::ir::IrBlock::Text {
                    text, citations, ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    let mut annotations =
                        super::super::url_citation_wire::url_annotations(text, 0, citations);
                    annotations.extend(super::file_annotations(citations));
                    let logprobs =
                        write_responses_part_logprobs(pending_logprobs.take().unwrap_or(&[]));

                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_MSG)?,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [{
                            (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                            (keys::TEXT): text,
                            (keys::ANNOTATIONS): annotations,
                            (keys::LOGPROBS): logprobs
                        }]
                    }));
                }
                crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                        // Native function_call items carry an item `id` (`fc_…`) distinct from
                        // `call_id`, as the streaming `done` emits; the IR has none, so synthesize one.
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_FC)?,
                        (CALL_ID): id,
                        (keys::NAME): name,
                        (keys::ARGUMENTS): args_str
                    }));
                }

                crate::codec::ir::IrBlock::Thinking {
                    text,
                    signature,
                    redacted,
                    kind,
                    ..
                } => {
                    if *redacted {
                        continue;
                    }
                    let emit_sig = signature.as_deref();

                    if text.is_empty() && emit_sig.is_none() {
                        continue;
                    }
                    let mut item = serde_json::Map::new();
                    item.insert(
                        keys::TYPE.to_string(),
                        serde_json::json!(ITEM_TYPE_REASONING),
                    );
                    item.insert(
                        keys::ID.to_string(),
                        serde_json::json!(synthesize_item_id(host, ITEM_ID_PREFIX_RS)?),
                    );

                    super::slots::insert_reasoning_text(&mut item, text, *kind);
                    if let Some(sig) = emit_sig {
                        item.insert(ENCRYPTED_CONTENT.to_string(), serde_json::json!(sig));
                    }
                    output_arr.push(serde_json::Value::Object(item));
                }
                crate::codec::ir::IrBlock::ToolResult { .. }
                | crate::codec::ir::IrBlock::Json(_) => {
                    drop_response_block(block);
                }

                crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. } => drop_response_block(block),

                crate::codec::ir::IrBlock::HostedToolRecord {
                    call_id,
                    status,
                    results,
                    ..
                } => output_arr.push(super::write_web_search_call(
                    host,
                    call_id.as_deref(),
                    status.as_deref(),
                    results,
                )?),
            }
        }

        let usage_value = build_responses_usage(&resp.usage);

        let mut obj = serde_json::Map::new();

        let id = resp
            .id
            .clone()
            .map(Ok)
            .unwrap_or_else(|| synthesize_response_id(host))?;
        let created_at = resp
            .created
            .unwrap_or_else(|| self.default_created_at(host));
        obj.insert(keys::ID.to_string(), serde_json::json!(id));
        obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
        obj.insert(keys::CREATED_AT.to_string(), serde_json::json!(created_at));
        obj.insert(keys::STATUS.to_string(), serde_json::json!(status));

        obj.insert(
            keys::MODEL.to_string(),
            serde_json::json!(resp.model.as_deref().unwrap_or(DEFAULT_MODEL)),
        );
        obj.insert(
            keys::OUTPUT.to_string(),
            serde_json::Value::Array(output_arr),
        );

        obj.insert(keys::USAGE.to_string(), usage_value);

        if let Some(tier) = resp
            .usage
            .detail
            .service_tier
            .as_deref()
            .and_then(|t| crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t))
        {
            obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
        }

        obj.insert(keys::ERROR_WORD.to_string(), if status == STATUS_FAILED {
            serde_json::json!({ (keys::CODE): ERR_TYPE_SERVER_ERROR, (keys::MESSAGE): keys::ERROR_WORD })
        } else {
            serde_json::Value::Null
        });

        if status == STATUS_INCOMPLETE {
            let reason = resp
                .stop_reason
                .map(write_responses_incomplete_reason)
                .unwrap_or(INCOMPLETE_REASON_OTHER);
            let mut incomplete_details = serde_json::Map::new();
            incomplete_details.insert(keys::REASON.to_string(), serde_json::json!(reason));
            obj.insert(
                INCOMPLETE_DETAILS.to_string(),
                serde_json::Value::Object(incomplete_details),
            );
        }

        fill_required_response_members(&mut obj, resp.request_echo.as_ref());

        Ok(serde_json::Value::Object(obj))
    }

    /// Native Responses error envelope, the OpenAI shape SDKs decode into `APIError`:
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        let error_type = match kind {
            "invalid_request" | ERR_TYPE_INVALID_REQUEST => ERR_TYPE_INVALID_REQUEST,
            "authentication" | ERR_TYPE_AUTHENTICATION | "auth" => ERR_TYPE_AUTHENTICATION,
            "permission" | ERR_TYPE_PERMISSION | "forbidden" => ERR_TYPE_PERMISSION,
            "not_found" | ERR_TYPE_NOT_FOUND => ERR_TYPE_NOT_FOUND,
            "rate_limit" | ERR_TYPE_RATE_LIMIT => ERR_TYPE_RATE_LIMIT,
            ERR_TYPE_SERVER_ERROR | "internal" | "internal_error" => ERR_TYPE_SERVER_ERROR,

            busbar_contract::protocol::KIND_OVERLOADED
            | ERR_TYPE_OVERLOADED
            | "service_unavailable"
            | "unavailable" => ERR_TYPE_SERVER_ERROR,

            crate::codec::framework::KIND_TIMEOUT
            | "network"
            | "connect"
            | "5xx"
            | "transient"
            | busbar_contract::protocol::KIND_API_ERROR => ERR_TYPE_SERVER_ERROR,

            crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH | "bad_request" => {
                ERR_TYPE_INVALID_REQUEST
            }
            "billing" | ERR_TYPE_INSUFFICIENT_QUOTA => ERR_TYPE_INSUFFICIENT_QUOTA,
            other => other,
        };

        serde_json::json!({
            (keys::ERROR_WORD): {
                (keys::MESSAGE): message,
                (keys::TYPE): error_type,
                (keys::CODE): bearer_error_code(error_type),
                "param": serde_json::Value::Null,
            }
        })
    }

    /// IR-18: `encrypted_content` is read back as an OpenAI-minted blob.
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        origin == crate::codec::ir::IrSignatureOrigin::OpenAi
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }
}

/// One IR image to a Responses `input_image` part, or `None` when the source has no Responses
/// form.
fn input_image_part(
    source: &crate::codec::ir::IrImageSource,
    detail: Option<crate::codec::ir::IrImageDetail>,
) -> Option<serde_json::Value> {
    let mut part = input_image_source_part(source)?;
    if let (Some(detail), Some(obj)) = (detail, part.as_object_mut()) {
        obj.insert(keys::DETAIL.to_string(), serde_json::json!(detail.as_str()));
    }
    Some(part)
}

/// The source half of [`input_image_part`].
fn input_image_source_part(source: &crate::codec::ir::IrImageSource) -> Option<serde_json::Value> {
    if let Some(id) = super::super::url_citation_wire::files_api_id(source) {
        return Some(serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::FILE_ID): id }));
    }
    match source {
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            value
                .get(keys::FILE_ID)
                .and_then(|i| i.as_str())
                .map(|id| serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::FILE_ID): id }))
        }

        crate::codec::ir::IrImageSource::Vendor { .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::IMAGE,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping unresolvable foreign vendor image reference on Responses egress: no \
                 cross-vendor analog");
            None
        }

        url_or_b64 => super::super::ir_encode::image_url_from_ir(url_or_b64)
            .map(|image_url| serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::IMAGE_URL): image_url })),
    }
}

/// One IR attachment to a Responses `input_file` part, or `None` when it has no Responses form.
fn input_file_part(
    kind: crate::codec::ir::IrMediaKind,
    source: &crate::codec::ir::IrImageSource,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    if kind != crate::codec::ir::IrMediaKind::Document {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::block(kind.as_str()),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [media_kind = kind.as_str(),],
            "dropping attachment on Responses egress: the input surface has an `input_file` part \
             and no audio or video part; the block is NOT emitted"
        );
        return None;
    }
    let mut part = serde_json::Map::new();
    part.insert(keys::TYPE.to_string(), serde_json::json!(INPUT_FILE));
    match source {
        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
            part.insert(
                keys::FILE_DATA.to_string(),
                serde_json::json!(format!("data:{media_type};base64,{data}")),
            );
        }
        crate::codec::ir::IrImageSource::Url(url)
        | crate::codec::ir::IrImageSource::UrlWithMime { url, .. } => {
            part.insert(FILE_URL.to_string(), serde_json::json!(url));
        }

        crate::codec::ir::IrImageSource::Vendor { .. }
            if super::super::url_citation_wire::files_api_id(source).is_some() =>
        {
            let id = super::super::url_citation_wire::files_api_id(source)?;
            part.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
        }

        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            let id = value.get(keys::FILE_ID).and_then(|i| i.as_str())?;
            part.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
        }
        crate::codec::ir::IrImageSource::Vendor { vendor, .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::DOCUMENT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [vendor = %vendor, ],
                "dropping document attachment on Responses egress: the source is a foreign vendor \
                 file handle this backend cannot resolve; the block is NOT emitted");
            return None;
        }
    }
    if let Some(n) = name {
        part.insert(keys::FILENAME.to_string(), serde_json::json!(n));
    }
    Some(serde_json::Value::Object(part))
}

/// A `ToolResult`'s content as the `function_call_output.output` value (RSP-10): a string, or an
/// array of `input_text` / `input_image` / `input_file` parts.
fn function_call_output_value(content: &[crate::codec::ir::IrBlock]) -> serde_json::Value {
    let text_of = |b: &crate::codec::ir::IrBlock| -> Option<String> {
        match b {
            crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),
            crate::codec::ir::IrBlock::Json(v) => Some(v.to_string()),
            _ => None,
        }
    };
    let has_attachment = content.iter().any(|b| {
        matches!(
            b,
            crate::codec::ir::IrBlock::Image { .. } | crate::codec::ir::IrBlock::Media { .. }
        )
    });
    if !has_attachment {
        return serde_json::json!(content.iter().filter_map(text_of).collect::<String>());
    }
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for (block_index, block) in content.iter().enumerate() {
        let _block_origin = crate::codec::drops::child_scope(block_index);

        match block {
            crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                parts.extend(input_image_part(source, *detail))
            }
            crate::codec::ir::IrBlock::Media {
                kind, source, name, ..
            } => parts.extend(input_file_part(*kind, source, name.as_deref())),
            other => {
                if let Some(text) = text_of(other) {
                    parts
                        .push(serde_json::json!({ (keys::TYPE): CONTENT_TYPE_INPUT_TEXT, (keys::TEXT): text }));
                }
            }
        }
    }
    if parts.is_empty() {
        return serde_json::json!("");
    }
    serde_json::Value::Array(parts)
}

/// The response surface cannot carry a generated attachment. The scoped source origin names the
/// loss; neither media bytes nor source URLs are included in diagnostics.
fn drop_response_block(block: &crate::codec::ir::IrBlock) {
    crate::codec::drops::writer_drop!(
        crate::codec::drops::block(block.kind_name()),
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        [],
        "dropping an answer block on Responses egress: no native output item"
    );
}
