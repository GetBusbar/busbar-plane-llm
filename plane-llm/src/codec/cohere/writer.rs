use super::*;

impl ProtocolWriter for CohereWriter {
    fn set_translate_cap(&self, cap: usize) {
        self.stream.lock().translate_cap = cap;
    }

    fn upstream_path(&self) -> &str {
        PATH_UPSTREAM
    }

    /// The typed request slots Cohere v2 `/chat` has no form for:
    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
            .map(crate::codec::carry::Slot::name)
            .chain(req.hosted_tools.iter().map(|h| h.kind_str()))
            .collect()
    }

    fn write_request_for_lane(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        _caps: &crate::codec::egress_prep::LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        let mut messages_arr: Vec<serde_json::Value> = Vec::new();

        let system_text: String = req
            .system
            .iter()
            .enumerate()
            .filter_map(|(block_index, b)| {
                let _block_origin =
                    crate::codec::drops::block_scope(req.block_origins.system.get(block_index));
                if let crate::codec::ir::IrBlock::Text { text, .. } = b {
                    Some(text.as_str())
                } else {



                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::block(b.kind_name()),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "dropping non-text system block on Cohere egress: Cohere v2 system prompt carries text only");
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !system_text.is_empty() {
            messages_arr.push(
                serde_json::json!({ (keys::ROLE): keys::SYSTEM, (keys::CONTENT): system_text }),
            );
        }

        let mut documents: Vec<serde_json::Value> = Vec::new();
        for (message_index, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(message_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let role_str = match msg.role {
                crate::codec::ir::IrRole::System => keys::SYSTEM,
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                crate::codec::ir::IrRole::Tool => keys::TOOL,
            };

            let mut parts: Vec<serde_json::Value> = Vec::new();
            let mut has_image = false;
            for (block_index, b) in msg.content.iter().enumerate() {
                let _block_origin =
                    crate::codec::drops::block_scope(_message_origins.get(block_index));

                match b {
                    crate::codec::ir::IrBlock::Text { text, .. } => {
                        parts.push(
                            serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): text }),
                        );
                    }

                    crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                        match super::super::ir_encode::image_url_from_ir(source) {
                            Some(url) => {
                                has_image = true;
                                let mut image_url = serde_json::Map::new();
                                image_url.insert(keys::URL.to_string(), serde_json::json!(url));

                                if let Some(d) = detail {
                                    image_url.insert(
                                        keys::DETAIL.to_string(),
                                        serde_json::json!(d.as_str()),
                                    );
                                }
                                parts.push(serde_json::json!({
                                    (keys::TYPE): keys::IMAGE_URL, (keys::IMAGE_URL): image_url
                                }));
                            }
                            None => crate::codec::drops::writer_drop!(
                                crate::codec::drops::IMAGE,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [],
                                "dropping unresolvable vendor-scoped image reference on Cohere \
                                 egress: a file_id / s3Location has no cross-vendor analog"
                            ),
                        }
                    }

                    crate::codec::ir::IrBlock::Media {
                        kind, source, name, ..
                    } => match (*kind == crate::codec::ir::IrMediaKind::Document)
                        .then(|| write_cohere_document(source, name.as_deref()))
                        .flatten()
                    {
                        Some(doc) => documents.push(doc),
                        None => crate::codec::drops::writer_drop!(
                            crate::codec::drops::block(kind.as_str()),
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [media_kind = kind.as_str(),],
                            "dropping attachment on Cohere egress: Cohere v2 /chat message \
                             content carries text and image parts only, and this attachment is \
                             not a text document the request's `documents` can carry — it is NOT \
                             emitted"
                        ),
                    },
                    _ => {}
                }
            }
            let text_blocks: Vec<&String> = msg
                .content
                .iter()
                .filter_map(|b| {
                    if let crate::codec::ir::IrBlock::Text { text, .. } = b {
                        Some(text)
                    } else {
                        None
                    }
                })
                .collect();

            let content_val: Option<serde_json::Value> = match text_blocks.as_slice() {
                _ if has_image => Some(serde_json::Value::Array(parts)),
                [] => None,
                [single] => Some(serde_json::Value::String((*single).clone())),
                _ => Some(serde_json::Value::Array(parts)),
            };

            let has_tool_result = msg
                .content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolResult { .. }));
            if msg.role == crate::codec::ir::IrRole::Tool || has_tool_result {
                let carrier_is_user = msg.role == crate::codec::ir::IrRole::User;
                let fold_text_into_tool = !carrier_is_user;
                let mut emitted_tool_result = false;
                for (block_index, block) in msg.content.iter().enumerate() {
                    let _block_origin =
                        crate::codec::drops::block_scope(_message_origins.get(block_index));

                    if let crate::codec::ir::IrBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        let mut tool_result_obj = serde_json::Map::new();
                        tool_result_obj
                            .insert(keys::ROLE.to_string(), serde_json::json!(keys::TOOL));
                        tool_result_obj.insert(
                            keys::TOOL_CALL_ID.to_string(),
                            serde_json::Value::String(tool_use_id.clone()),
                        );
                        let mut text_parts: Vec<String> =
                            if fold_text_into_tool && !emitted_tool_result {
                                text_blocks.iter().map(|text| (*text).clone()).collect()
                            } else {
                                Vec::new()
                            };
                        text_parts.extend(content.iter().filter_map(|b| match b {
                            crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),

                            crate::codec::ir::IrBlock::Json(v) => {
                                crate::codec::json::to_string(v).ok()
                            }
                            _ => None,
                        }));

                        let mut doc_parts: Vec<serde_json::Value> = Vec::new();
                        for (block_index, b) in content.iter().enumerate() {
                            let _block_origin = crate::codec::drops::child_scope(block_index);

                            let (kind, source) = match b {
                                crate::codec::ir::IrBlock::Media { kind, source, .. } => {
                                    (kind.as_str(), source)
                                }
                                crate::codec::ir::IrBlock::Text { .. }
                                | crate::codec::ir::IrBlock::Json(_) => continue,
                                _ => {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::block(b.kind_name()),
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [],
                                        "dropping non-text block inside a tool result on Cohere egress: tool content carries text and native document parts only"
                                    );
                                    continue;
                                }
                            };
                            match source {
                                crate::codec::ir::IrImageSource::Vendor { vendor, value }
                                    if *vendor == VENDOR_NAME =>
                                {
                                    doc_parts.push(serde_json::json!({
                                        (keys::TYPE): keys::DOCUMENT,
                                        (keys::DOCUMENT): value
                                    }));
                                }
                                _ => crate::codec::drops::writer_drop!(
                                    crate::codec::drops::block(kind),
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [media_kind = kind,],
                                    "dropping attachment inside a tool result on Cohere egress: \
                                     Cohere v2 tool content carries text and native `document` \
                                     parts only, and this source has no Cohere document form"
                                ),
                            }
                        }

                        let joined = text_parts.join("");
                        let content_value = if doc_parts.is_empty() {
                            serde_json::Value::String(joined)
                        } else {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            if !joined.is_empty() {
                                parts.push(serde_json::json!({(keys::TYPE): keys::TEXT, (keys::TEXT): joined}));
                            }
                            parts.extend(doc_parts);
                            serde_json::Value::Array(parts)
                        };
                        tool_result_obj.insert(keys::CONTENT.to_string(), content_value);
                        messages_arr.push(serde_json::Value::Object(tool_result_obj));
                        emitted_tool_result = true;
                    }
                }
                if carrier_is_user {
                    if let Some(user_content) = content_val {
                        let mut user_obj = serde_json::Map::new();
                        user_obj.insert(keys::ROLE.to_string(), serde_json::json!(keys::USER));
                        user_obj.insert(keys::CONTENT.to_string(), user_content);
                        messages_arr.push(serde_json::Value::Object(user_obj));
                    }
                } else if !emitted_tool_result && !text_blocks.is_empty() {
                    let mut tool_obj = serde_json::Map::new();
                    tool_obj.insert(keys::ROLE.to_string(), serde_json::json!(keys::TOOL));
                    tool_obj.insert(
                        keys::CONTENT.to_string(),
                        serde_json::Value::String(
                            text_blocks
                                .iter()
                                .map(|t| t.as_str())
                                .collect::<Vec<&str>>()
                                .join(""),
                        ),
                    );
                    messages_arr.push(serde_json::Value::Object(tool_obj));
                }
                continue;
            }

            let mut msg_obj = serde_json::Map::new();
            msg_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));
            if let Some(content_val) = content_val {
                msg_obj.insert(keys::CONTENT.to_string(), content_val);
            }

            if msg.role == crate::codec::ir::IrRole::Assistant {
                let thinking: Vec<&str> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::codec::ir::IrBlock::Thinking {
                            text,
                            redacted: false,
                            ..
                        } if !text.is_empty() => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let has_tool_calls = msg
                    .content
                    .iter()
                    .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. }));
                if has_tool_calls {
                    let plan: String = thinking.concat();
                    if !plan.is_empty() {
                        msg_obj.insert(TOOL_PLAN.to_string(), serde_json::json!(plan));
                    }
                } else if !thinking.is_empty() {
                    let mut parts: Vec<serde_json::Value> = thinking
                        .iter()
                        .map(|t| serde_json::json!({ (keys::TYPE): keys::THINKING, (keys::THINKING): t }))
                        .collect();
                    match msg_obj.remove(keys::CONTENT) {
                        Some(serde_json::Value::String(text)) => {
                            parts.push(
                                serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): text }),
                            );
                        }
                        Some(serde_json::Value::Array(rest)) => parts.extend(rest),
                        _ => {}
                    }
                    msg_obj.insert(keys::CONTENT.to_string(), serde_json::Value::Array(parts));
                }
            }

            let msg_citations = write_cohere_citations(&msg.content);
            if !msg_citations.is_empty() {
                msg_obj.insert(
                    keys::CITATIONS.to_string(),
                    serde_json::Value::Array(msg_citations),
                );
            }

            if msg.role == crate::codec::ir::IrRole::Assistant {
                let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();
                for (block_index, block) in msg.content.iter().enumerate() {
                    let _block_origin =
                        crate::codec::drops::block_scope(_message_origins.get(block_index));

                    if let crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                        tool_calls_arr.push(serde_json::json!({ (keys::ID): id, (keys::TYPE): keys::FUNCTION, (keys::FUNCTION): { (keys::NAME): name, (keys::ARGUMENTS): args_str }}));
                    }
                }
                if !tool_calls_arr.is_empty() {
                    msg_obj.insert(
                        keys::TOOL_CALLS.to_string(),
                        serde_json::Value::Array(tool_calls_arr),
                    );
                }
            }

            messages_arr.push(serde_json::Value::Object(msg_obj));
        }

        out.insert(
            keys::MESSAGES.to_string(),
            serde_json::Value::Array(messages_arr),
        );

        let tools: Vec<&crate::codec::ir::IrTool> = match &req.allowed_tools {
            Some(allowed) => req
                .tools
                .iter()
                .filter(|t| allowed.contains(&t.name))
                .collect(),
            None => req.tools.iter().collect(),
        };
        let strict: Vec<Option<bool>> = tools.iter().map(|t| t.strict).collect();
        match strict.first() {
            Some(Some(first)) if strict.iter().all(|s| *s == Some(*first)) => {
                out.insert(STRICT_TOOLS.to_string(), serde_json::json!(first));
            }
            _ => super::super::ir_encode::warn_dropped_tool_strict(&req.tools, VENDOR_NAME),
        }
        if !tools.is_empty() {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in &tools {
                let mut func_obj = serde_json::Map::new();
                func_obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));
                if let Some(desc) = &tool.description {
                    func_obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }
                let params = if !tool.input_schema.is_null() {
                    tool.input_schema.clone()
                } else {
                    serde_json::json!({})
                };
                func_obj.insert(keys::PARAMETERS.to_string(), params);
                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::FUNCTION));
                tool_obj.insert(
                    keys::FUNCTION.to_string(),
                    serde_json::Value::Object(func_obj),
                );
                tools_arr.push(serde_json::Value::Object(tool_obj));
            }
            out.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }

        if let Some(tc) = &req.tool_choice {
            if tools.is_empty() {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Cohere egress: tool_choice has no accompanying tools \
                     (likely because the hosted tools that carried it were stripped on the \
                     cross-protocol seam)"
                );
            } else {
                let v = match tc {
                    crate::codec::ir::IrToolChoice::Required => Some(COHERE_TOOL_CHOICE_REQUIRED),
                    crate::codec::ir::IrToolChoice::Tool { name } => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::TOOL_CHOICE,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [tool = %name, ],
                            "dropping the targeted tool constraint on Cohere egress: tool_choice \
                             can require a tool but cannot select a particular function"
                        );
                        Some(COHERE_TOOL_CHOICE_REQUIRED)
                    }
                    crate::codec::ir::IrToolChoice::None => Some(COHERE_TOOL_CHOICE_NONE),
                    crate::codec::ir::IrToolChoice::Auto => None,
                };
                if let Some(s) = v {
                    out.insert(keys::TOOL_CHOICE.to_string(), serde_json::json!(s));
                }
            }
        }

        if req.parallel_tool_calls.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::PARALLEL_TOOL_CALLS,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping parallel_tool_calls on Cohere egress: /v2/chat has no parallelism \
                 control, so the backend's default parallelism applies"
            );
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

        if let Some(max_tokens) = req.max_tokens {
            out.insert(keys::MAX_TOKENS.to_string(), serde_json::json!(max_tokens));
        }

        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );

        if let Some(logprobs) = req.logprobs {
            out.insert(keys::LOGPROBS.to_string(), serde_json::json!(logprobs));
        }

        if let Some(response_format) = &req.response_format {
            out.insert(
                keys::RESPONSE_FORMAT.to_string(),
                write_cohere_response_format(response_format),
            );
        }

        if req.stream {
            out.insert(keys::STREAM.to_string(), serde_json::json!(true));
        }

        if let Some(ask) = req.reasoning {
            out.insert(
                keys::THINKING.to_string(),
                write_cohere_reasoning(
                    ask,
                    req.reasoning_budgets
                        .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS),
                ),
            );
        }
        for (key, value) in &req.extra {
            out.insert(key.clone(), value.clone());
        }

        if !documents.is_empty() {
            match out.get_mut(keys::DOCUMENTS).and_then(|d| d.as_array_mut()) {
                Some(existing) => existing.extend(documents),
                None => {
                    out.insert(
                        keys::DOCUMENTS.to_string(),
                        serde_json::Value::Array(documents),
                    );
                }
            }
        }

        serde_json::Value::Object(out)
    }

    fn write_response_event(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match ev {
            IrStreamEvent::MessageStart { role, id, .. } => {
                self.begin_stream();
                let cohere_role = match role {
                    crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                    crate::codec::ir::IrRole::System
                    | crate::codec::ir::IrRole::User
                    | crate::codec::ir::IrRole::Tool => return None,
                };

                let id = match self.carried_stream_id(|| {
                    id.clone()
                        .filter(|id| !id.is_empty())
                        .map(Ok)
                        .unwrap_or_else(|| synthesize_cohere_id(host))
                }) {
                    Ok(id) => id,
                    Err(_) => return Some((String::new(), self.encoding_failure(host))),
                };
                Some((
                    "".to_string(),
                    serde_json::json!({
                        (keys::ID): id,
                        (keys::TYPE): ET_MESSAGE_START,
                        (keys::DELTA): { (keys::MESSAGE): { (keys::ROLE): cohere_role } }
                    }),
                ))
            }

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => match block {
                crate::codec::ir::IrBlockMeta::Text => {
                    let index = self.mark_text_open(*index);
                    Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_CONTENT_START,
                            (keys::INDEX): index,
                            (keys::DELTA): {
                                (keys::MESSAGE): {
                                    (keys::CONTENT): { (keys::TYPE): keys::TEXT, (keys::TEXT): "" }
                                }
                            }
                        }),
                    ))
                }

                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    let index = self.mark_tool_open(*index);
                    Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_TOOL_CALL_START,
                            (keys::INDEX): index,
                            (keys::DELTA): {
                                (keys::MESSAGE): {
                                    (keys::TOOL_CALLS): {
                                        (keys::ID): id,
                                        (keys::TYPE): keys::FUNCTION,
                                        (keys::FUNCTION): { (keys::NAME): name, (keys::ARGUMENTS): "" }
                                    }
                                }
                            }
                        }),
                    ))
                }

                crate::codec::ir::IrBlockMeta::Thinking { .. } => {
                    let index = self.mark_text_open(*index);
                    Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_CONTENT_START,
                            (keys::INDEX): index,
                            (keys::DELTA): {
                                (keys::MESSAGE): {
                                    (keys::CONTENT): { (keys::TYPE): keys::THINKING, (keys::THINKING): "" }
                                }
                            }
                        }),
                    ))
                }

                crate::codec::ir::IrBlockMeta::RedactedThinking
                | crate::codec::ir::IrBlockMeta::Image => None,
            },

            IrStreamEvent::BlockDelta { index, delta } => {
                let ir_index = *index;
                if let crate::codec::ir::IrDelta::TextDelta(text) = delta {
                    self.record_citation_text(ir_index, text);
                }
                let index = self.wire_index(ir_index);
                match delta {
                    crate::codec::ir::IrDelta::TextDelta(text) => Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_CONTENT_DELTA,
                            (keys::INDEX): index,
                            (keys::DELTA): { (keys::MESSAGE): { (keys::CONTENT): { (keys::TEXT): text } } }
                        }),
                    )),

                    crate::codec::ir::IrDelta::InputJsonDelta(args) => Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_TOOL_CALL_DELTA,
                            (keys::INDEX): index,
                            (keys::DELTA): {
                                (keys::MESSAGE): {
                                    (keys::TOOL_CALLS): { (keys::FUNCTION): { (keys::ARGUMENTS): args } }
                                }
                            }
                        }),
                    )),

                    crate::codec::ir::IrDelta::ThinkingDelta(text) => Some((
                        "".to_string(),
                        serde_json::json!({
                            (keys::TYPE): ET_CONTENT_DELTA,
                            (keys::INDEX): index,
                            (keys::DELTA): { (keys::MESSAGE): { (keys::CONTENT): { (keys::THINKING): text } } }
                        }),
                    )),

                    crate::codec::ir::IrDelta::SignatureDelta(_)
                    | crate::codec::ir::IrDelta::RedactedReasoningDelta(_) => None,
                    crate::codec::ir::IrDelta::MediaDelta(block) => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::block(block.kind_name()),
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping generated media on Cohere streaming egress: no native media delta"
                        );
                        None
                    }

                    crate::codec::ir::IrDelta::CitationsDelta(cits) if !cits.is_empty() => {
                        let c = cits.first()?;
                        let span = self.citation_span(ir_index, c);
                        let Some(citation) = write_cohere_citation(c, span) else {
                            return self.write_response_event(
                                host,
                                &IrStreamEvent::Error(response_projection_error()),
                            );
                        };
                        let Some(index) = self.next_citation_index() else {
                            return self.write_response_event(
                                host,
                                &IrStreamEvent::Error(response_projection_error()),
                            );
                        };
                        Some((
                            "".to_string(),
                            serde_json::json!({
                                (keys::TYPE): ET_CITATION_START,
                                (keys::INDEX): index,
                                (keys::DELTA): {
                                    (keys::MESSAGE): {
                                        (keys::CITATIONS): citation
                                    }
                                }
                            }),
                        ))
                    }
                    crate::codec::ir::IrDelta::CitationsDelta(_) => None,

                    crate::codec::ir::IrDelta::LogprobsDelta(_) => None,
                }
            }

            IrStreamEvent::BlockStop { index } => {
                let wire_index = self.wire_index(*index);

                if self.take_tool_open(*index) {
                    Some((
                        "".to_string(),
                        serde_json::json!({ (keys::TYPE): ET_TOOL_CALL_END, (keys::INDEX): wire_index }),
                    ))
                } else if self.take_text_open(*index) {
                    Some((
                        "".to_string(),
                        serde_json::json!({ (keys::TYPE): ET_CONTENT_END, (keys::INDEX): wire_index }),
                    ))
                } else {
                    None
                }
            }

            IrStreamEvent::MessageDelta {
                stop_reason,
                usage,
                stop_sequence: _,
                stop_detail: _,
            } => {
                let cohere_finish_reason = stop_reason
                    .map(write_cohere_stop_reason)
                    .unwrap_or(COHERE_FINISH_COMPLETE);

                let mut usage_obj = serde_json::json!({
                    (keys::TOKENS): {
                        (keys::INPUT_TOKENS): cohere_prompt_tokens(usage),
                        (keys::OUTPUT_TOKENS): usage.output_tokens
                    }
                });

                if let (Some(cached), Some(uo)) = (
                    usage.cache_read_input_tokens.filter(|n| *n > 0),
                    usage_obj.as_object_mut(),
                ) {
                    uo.insert(keys::CACHED_TOKENS.to_string(), serde_json::json!(cached));
                }

                let mut billed_units = serde_json::Map::new();
                if let Some(v) = usage.detail.billed_input_tokens {
                    billed_units.insert(keys::INPUT_TOKENS.to_string(), serde_json::json!(v));
                }
                if let Some(v) = usage.detail.billed_output_tokens {
                    billed_units.insert(keys::OUTPUT_TOKENS.to_string(), serde_json::json!(v));
                }
                if let Some(su) = usage.detail.search_units {
                    billed_units.insert(keys::SEARCH_UNITS.to_string(), serde_json::json!(su));
                }
                if let Some(v) = usage.detail.billed_classifications {
                    billed_units.insert(CLASSIFICATIONS.to_string(), serde_json::json!(v));
                }
                if !billed_units.is_empty() {
                    if let Some(uo) = usage_obj.as_object_mut() {
                        uo.insert(
                            keys::BILLED_UNITS.to_string(),
                            serde_json::Value::Object(billed_units),
                        );
                    }
                }
                Some((
                    "".to_string(),
                    serde_json::json!({
                        (keys::TYPE): ET_MESSAGE_END,
                        (keys::DELTA): {
                            (keys::FINISH_REASON): cohere_finish_reason,
                            (keys::USAGE): usage_obj
                        }
                    }),
                ))
            }

            IrStreamEvent::MessageStop => {
                self.finish_stream();
                None
            }
            IrStreamEvent::Error(err) => {
                self.finish_stream();

                let finish_reason = COHERE_FINISH_ERROR;

                if err.provider_signal.is_some() {
                    tracing::warn!(
                        finish_reason,
                        "cohere: mid-stream error terminating with native message-end frame"
                    );
                }
                Some((
                    "".to_string(),
                    serde_json::json!({
                        (keys::TYPE): ET_MESSAGE_END,
                        (keys::DELTA): {
                            (keys::FINISH_REASON): finish_reason,
                            (keys::USAGE): {
                                (keys::TOKENS): { (keys::INPUT_TOKENS): 0, (keys::OUTPUT_TOKENS): 0 }
                            }
                        }
                    }),
                ))
            }
        }
    }

    fn write_response_events(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        if let IrStreamEvent::BlockDelta {
            index,
            delta: crate::codec::ir::IrDelta::CitationsDelta(cits),
        } = ev
        {
            if cits
                .iter()
                .any(|citation| !self.stream_citation_representable(*index, citation))
            {
                return self
                    .write_response_event(host, &IrStreamEvent::Error(response_projection_error()))
                    .into_iter()
                    .collect();
            }
            let mut frames = Vec::with_capacity(cits.len() * 2);

            for c in cits {
                let one = IrStreamEvent::BlockDelta {
                    index: *index,
                    delta: crate::codec::ir::IrDelta::CitationsDelta(vec![c.clone()]),
                };
                if let Some(start) = self.write_response_event(host, &one) {
                    if start.1[keys::TYPE] != ET_CITATION_START {
                        frames.push(start);
                        break;
                    }
                    let citation_index = start.1[keys::INDEX].clone();
                    frames.push(start);
                    frames.push((
                        "".to_string(),
                        serde_json::json!({ (keys::TYPE): ET_CITATION_END, (keys::INDEX): citation_index }),
                    ));
                }
            }
            return frames;
        }
        self.write_response_event(host, ev).into_iter().collect()
    }

    fn write_response(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        self.try_write_response(host, resp)
            .unwrap_or_else(|_| self.encoding_failure(host))
    }

    fn try_write_response_events(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        ev: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, IrError> {
        match ev {
            IrStreamEvent::BlockStart {
                block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
                ..
            } if id.is_empty() || name.is_empty() => return Err(response_projection_error()),
            IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
            } if citations
                .iter()
                .any(|citation| !self.stream_citation_representable(*index, citation)) =>
            {
                return Err(response_projection_error());
            }
            _ => {}
        }

        if let IrStreamEvent::MessageStart {
            role: crate::codec::ir::IrRole::Assistant,
            id,
            ..
        } = ev
        {
            self.begin_stream();
            self.carried_stream_id(|| {
                id.clone()
                    .filter(|id| !id.is_empty())
                    .map(Ok)
                    .unwrap_or_else(|| synthesize_cohere_id(host))
            })?;
        }
        Ok(self.write_response_events(host, ev))
    }

    fn try_write_response(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> Result<serde_json::Value, IrError> {
        let mut out = serde_json::Map::new();
        let mut content_arr: Vec<serde_json::Value> = Vec::new();
        let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();

        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            match block {
                crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let bad_arguments = match input {
                        serde_json::Value::String(arguments) => {
                            crate::codec::json::parse_str::<serde_json::Value>(arguments).is_err()
                        }
                        _ => false,
                    };
                    if id.is_empty() || name.is_empty() || bad_arguments {
                        return Err(response_projection_error());
                    }
                }
                crate::codec::ir::IrBlock::Text { citations, .. }
                    if citations
                        .iter()
                        .any(|citation| !citation_representable(citation)) =>
                {
                    return Err(response_projection_error());
                }
                _ => {}
            }
        }
        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            match block {
                crate::codec::ir::IrBlock::Text { text, .. } => {
                    content_arr
                        .push(serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): text }));
                }
                crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let args_str = crate::codec::dialect::tool_arguments_to_string(input);

                    tool_calls_arr.push(serde_json::json!({ (keys::ID): id, (keys::TYPE): keys::FUNCTION, (keys::FUNCTION): { (keys::NAME): name, (keys::ARGUMENTS): args_str }}));
                }

                crate::codec::ir::IrBlock::Thinking {
                    text,
                    redacted: false,
                    ..
                } => {
                    if !text.is_empty() {
                        content_arr
                            .push(serde_json::json!({ (keys::TYPE): keys::THINKING, (keys::THINKING): text }));
                    }
                }
                crate::codec::ir::IrBlock::Thinking { .. } => {}
                crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. }
                | crate::codec::ir::IrBlock::ToolResult { .. }
                | crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
            }
        }

        let cohere_finish_reason = resp
            .stop_reason
            .map(write_cohere_stop_reason)
            .unwrap_or(COHERE_FINISH_COMPLETE);

        if !resp.logprobs.is_empty() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::LOGPROBS,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping response logprobs on Cohere egress: a Cohere logprobs item needs the token \
                 ids, which the source dialect has no field for"
            );
        }

        let mut tokens_map = serde_json::Map::new();

        tokens_map.insert(
            keys::INPUT_TOKENS.to_string(),
            serde_json::json!(cohere_prompt_tokens(&resp.usage)),
        );
        tokens_map.insert(
            keys::OUTPUT_TOKENS.to_string(),
            serde_json::json!(resp.usage.output_tokens),
        );

        let id = resp
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .map(Ok)
            .unwrap_or_else(|| synthesize_cohere_id(host))?;
        out.insert(keys::ID.to_string(), serde_json::Value::String(id));

        if let Some(ref model) = resp.model {
            out.insert(keys::MODEL.to_string(), serde_json::json!(model));
        }
        out.insert(
            keys::FINISH_REASON.to_string(),
            serde_json::json!(cohere_finish_reason),
        );

        let mut message_obj = serde_json::Map::new();
        message_obj.insert(keys::ROLE.to_string(), serde_json::json!(keys::ASSISTANT));
        message_obj.insert(
            keys::CONTENT.to_string(),
            serde_json::Value::Array(content_arr),
        );

        let citations_arr = write_cohere_citations(&resp.content);
        if !citations_arr.is_empty() {
            message_obj.insert(
                keys::CITATIONS.to_string(),
                serde_json::Value::Array(citations_arr),
            );
        }
        if !tool_calls_arr.is_empty() {
            message_obj.insert(
                keys::TOOL_CALLS.to_string(),
                serde_json::Value::Array(tool_calls_arr),
            );
        }
        out.insert(
            keys::MESSAGE.to_string(),
            serde_json::Value::Object(message_obj),
        );

        let mut usage_map = serde_json::Map::new();
        usage_map.insert(
            keys::TOKENS.to_string(),
            serde_json::Value::Object(tokens_map),
        );

        if let Some(cached) = resp.usage.cache_read_input_tokens.filter(|n| *n > 0) {
            usage_map.insert(keys::CACHED_TOKENS.to_string(), serde_json::json!(cached));
        }

        let mut billed_units = serde_json::Map::new();
        if let Some(v) = resp.usage.detail.billed_input_tokens {
            billed_units.insert(keys::INPUT_TOKENS.to_string(), serde_json::json!(v));
        }
        if let Some(v) = resp.usage.detail.billed_output_tokens {
            billed_units.insert(keys::OUTPUT_TOKENS.to_string(), serde_json::json!(v));
        }
        if let Some(su) = resp.usage.detail.search_units {
            billed_units.insert(keys::SEARCH_UNITS.to_string(), serde_json::json!(su));
        }
        if let Some(v) = resp.usage.detail.billed_classifications {
            billed_units.insert(CLASSIFICATIONS.to_string(), serde_json::json!(v));
        }
        if !billed_units.is_empty() {
            usage_map.insert(
                keys::BILLED_UNITS.to_string(),
                serde_json::Value::Object(billed_units),
            );
        }
        out.insert(
            keys::USAGE.to_string(),
            serde_json::Value::Object(usage_map),
        );

        Ok(serde_json::Value::Object(out))
    }

    /// NATIVE Cohere v2 error envelope. The Cohere v2 chat API conveys the error *category* via the
    /// HTTP status (400/401/404/429/5xx) and carries only a human-readable `{"message": <detail>}`
    /// body — it has no typed `error.type`/`code` field the way OpenAI/Anthropic do.
    fn write_error(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        _status: u16,
        _kind: &str,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            (keys::MESSAGE): message,
        })
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }
}
