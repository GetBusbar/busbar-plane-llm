use super::*;

/// The `created` stamped when the answer carries none: the host's wall clock via the
/// [`CodecHost`](crate::codec::cells::CodecHost) (the plane reads no clock of its own), or `0`
/// (what an unstamped Responses answer carries) when the host holds none.
fn host_now(host: &dyn crate::codec::cells::CodecHost) -> u64 {
    host.wall_clock_now().unwrap_or(0)
}

/// One streamed `chat.completion.chunk` frame carrying `delta` on its single choice, unfinished.
fn delta_chunk(delta: serde_json::Value) -> Option<(String, serde_json::Value)> {
    Some((
        String::new(),
        serde_json::json!({
            (keys::OBJECT): OBJ_CHUNK,
            (CHOICES): [{
                (keys::INDEX): 0,
                (keys::DELTA): delta,
                (keys::FINISH_REASON): null
            }]
        }),
    ))
}

impl ProtocolWriter for OpenAiWriter {
    fn upstream_path(&self) -> &str {
        PATH_UPSTREAM
    }

    /// The candidate count is top-level `n`. `Some(k)` only for `k > 1`; absent, 1 or unparseable
    /// is `None`.
    fn requested_candidate_count(&self, body: &serde_json::Value) -> Option<u64> {
        body.get(W_N).and_then(|v| v.as_u64()).filter(|&k| k > 1)
    }

    /// A hosted tool Chat has no built-in for (code execution, web fetch) is dropped by the writer;
    /// the seam records each as a dropped control.
    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
            .map(crate::codec::carry::Slot::name)
            .chain(super::slots::dropped_hosted_kinds(req))
            .collect()
    }

    fn write_request_for_lane(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        crate::codec::carry::warn_drops(super::map::REQUEST, super::map::CONTROLS, None, req);
        let mut messages_array: Vec<serde_json::Value> = Vec::new();

        let system_role = req
            .system_role
            .map_or(keys::SYSTEM, crate::codec::ir::IrSystemRole::as_str);
        for (block_index, block) in req.system.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(req.block_origins.system.get(block_index));

            let text: &str = match block {
                crate::codec::ir::IrBlock::Text { text, .. } => text,
                crate::codec::ir::IrBlock::Thinking { text, .. } => text,
                crate::codec::ir::IrBlock::ToolUse { .. }
                | crate::codec::ir::IrBlock::ToolResult { .. }
                | crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. }
                | crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => "",
            };
            messages_array.push(serde_json::json!({
                (keys::ROLE): system_role,
                (keys::CONTENT): text
            }));
        }

        let message_names = req
            .extra
            .get(MESSAGE_NAMES_SENTINEL)
            .and_then(|v| v.as_object());

        let message_extras = req
            .extra
            .get(MESSAGE_EXTRAS_SENTINEL)
            .and_then(|v| v.as_object());

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
                crate::codec::ir::IrRole::Tool => keys::TOOL,
                crate::codec::ir::IrRole::System => keys::SYSTEM,
            };

            let content_val: serde_json::Value = if msg.content.is_empty() {
                serde_json::json!("")
            } else {
                let mut content_arr: Vec<serde_json::Value> = Vec::new();

                for (block_index, block) in msg.content.iter().enumerate() {
                    let _block_origin =
                        crate::codec::drops::block_scope(_message_origins.get(block_index));

                    match block {
                        crate::codec::ir::IrBlock::Text {
                            text,
                            refusal: true,
                            ..
                        } if msg.role == crate::codec::ir::IrRole::Assistant => {
                            content_arr
                                .push(serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text }));
                        }
                        crate::codec::ir::IrBlock::Text { text, .. } => {
                            content_arr.push(
                                serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): text }),
                            );
                        }
                        crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                            match super::super::ir_encode::image_url_from_ir(source) {
                                Some(url) => {
                                    let mut image_url = serde_json::json!({ (keys::URL): url });

                                    if let Some(d) = detail {
                                        image_url[keys::DETAIL] = serde_json::json!(d.as_str());
                                    }
                                    content_arr.push(serde_json::json!({
                                        (keys::TYPE): keys::IMAGE_URL,
                                        (keys::IMAGE_URL): image_url
                                    }))
                                }
                                None => crate::codec::drops::writer_drop!(
                                    crate::codec::drops::IMAGE,
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [],
                                    "dropping unresolvable vendor-scoped image reference on OpenAI \
                                     egress: a Responses input_image.file_id or a Bedrock s3Location \
                                     has no cross-vendor analog; the block is NOT emitted"
                                ),
                            }
                        }
                        crate::codec::ir::IrBlock::Media {
                            kind, source, name, ..
                        } => {
                            if let Some(part) =
                                super::media_part_from_ir(*kind, source, name.as_deref())
                            {
                                content_arr.push(part);
                            }
                        }
                        crate::codec::ir::IrBlock::ToolUse { .. } => {}
                        crate::codec::ir::IrBlock::ToolResult { .. } => {}
                        crate::codec::ir::IrBlock::Thinking { .. } => {}
                        crate::codec::ir::IrBlock::Json(_)
                        | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
                    }
                }

                if content_arr.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Array(content_arr)
                }
            };

            let mut msg_obj = serde_json::json!({
                (keys::ROLE): role_str,
                (keys::CONTENT): content_val,
            });

            if let Some(name) = message_names
                .and_then(|m| m.get(&msg_idx.to_string()))
                .and_then(|v| v.as_str())
            {
                if let Some(o) = msg_obj.as_object_mut() {
                    o.insert(keys::NAME.to_string(), serde_json::json!(name));
                }
            }

            if let Some(ex) = message_extras
                .and_then(|m| m.get(&msg_idx.to_string()))
                .and_then(|v| v.as_object())
            {
                if let Some(o) = msg_obj.as_object_mut() {
                    for (k, v) in ex {
                        if k == LEGACY_FUNCTION_ROLE_KEY {
                            continue;
                        }
                        o.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }

            {
                let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();
                for (block_index, block) in msg.content.iter().enumerate() {
                    let _block_origin =
                        crate::codec::drops::block_scope(_message_origins.get(block_index));

                    if let crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        let args_str = crate::codec::dialect::tool_arguments_to_string(input);

                        tool_calls_arr.push(serde_json::json!({
                            (keys::TYPE): TOOL_TYPE_FUNCTION,
                            (keys::ID): id,
                            (keys::FUNCTION): {
                                (keys::NAME): name,
                                (keys::ARGUMENTS): args_str
                            }
                        }));
                    }
                }

                if !tool_calls_arr.is_empty() && msg_obj.get(keys::FUNCTION_CALL).is_none() {
                    msg_obj[keys::TOOL_CALLS] = serde_json::Value::Array(tool_calls_arr);
                }
            }

            let has_tool_result = msg
                .content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolResult { .. }));
            if has_tool_result {
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
                        let mut tool_result_obj = serde_json::json!({
                            (keys::ROLE): keys::TOOL,
                            (keys::TOOL_CALL_ID): tool_use_id,
                            (keys::CONTENT): "",
                        });

                        if !content.is_empty() {
                            let text_parts: Vec<String> = content
                                .iter()
                                .enumerate()
                                .filter_map(|(index, b)| {
                                    let _origin = crate::codec::drops::child_scope(index);
                                    match b {
                                        crate::codec::ir::IrBlock::Text { text, .. } => {
                                            Some(text.clone())
                                        }

                                        crate::codec::ir::IrBlock::Json(v) => Some(v.to_string()),

                                        _ => {
                                            crate::codec::drops::writer_drop!(
                                                crate::codec::drops::block(b.kind_name()),
                                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                                [],
                                                "dropping non-text block inside a tool result on OpenAI Chat egress: tool content carries text only"
                                            );
                                            None
                                        }
                                    }
                                })
                                .collect();

                            tool_result_obj[keys::CONTENT] = serde_json::json!(text_parts.concat());
                        }

                        if let Some(fname) = message_extras
                            .and_then(|m| m.get(&msg_idx.to_string()))
                            .and_then(|e| e.get(LEGACY_FUNCTION_ROLE_KEY))
                        {
                            tool_result_obj = serde_json::json!({
                                (keys::ROLE): keys::FUNCTION,
                                (keys::NAME): fname,
                                (keys::CONTENT): tool_result_obj[keys::CONTENT].clone(),
                            });
                        }
                        messages_array.push(tool_result_obj);
                        emitted_tool_result = true;
                    }
                }

                let msg_has_payload = msg_obj.get(keys::CONTENT).is_some_and(|c| !c.is_null())
                    || msg_obj.get(keys::TOOL_CALLS).is_some();
                if msg_has_payload || !emitted_tool_result {
                    messages_array.push(msg_obj);
                }
            } else {
                if let Some(o) = msg_obj.as_object_mut() {
                    let content_null = o.get(keys::CONTENT).is_some_and(|c| c.is_null());
                    if content_null
                        && !o.contains_key(keys::TOOL_CALLS)
                        && !o.contains_key(keys::FUNCTION_CALL)
                    {
                        o.insert(keys::CONTENT.to_string(), serde_json::json!(""));
                    }
                }
                messages_array.push(msg_obj);
            }
        }

        let mut out = serde_json::Map::new();

        if let Some(model_val) = req.extra.get(keys::MODEL) {
            out.insert(keys::MODEL.to_string(), model_val.clone());
        }

        out.insert(
            keys::MESSAGES.to_string(),
            serde_json::Value::Array(messages_array),
        );

        if let Some(max_tokens) = req.max_tokens {
            let completion_key = req
                .extra
                .get(MAX_COMPLETION_TOKENS_SENTINEL)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || caps.max_output_key == MaxOutputKey::MaxCompletionTokens;
            let key = if completion_key {
                MAX_COMPLETION_TOKENS
            } else {
                keys::MAX_TOKENS
            };
            out.insert(key.to_string(), serde_json::json!(max_tokens));
        }

        if let Some(user) = &req.user {
            out.insert(keys::USER.to_string(), serde_json::json!(user));
        }

        let has_callable_tools = !req.tools.is_empty()
            || req
                .hosted_tools
                .iter()
                .any(|tool| matches!(tool, crate::codec::ir::IrHostedTool::Custom(_)));
        if let (Some(parallel), true) = (req.parallel_tool_calls, has_callable_tools) {
            out.insert(
                keys::PARALLEL_TOOL_CALLS.to_string(),
                serde_json::json!(parallel),
            );
        }

        if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) && caps.reasoning_none {
            out.insert(
                REASONING_EFFORT.to_string(),
                serde_json::json!(keys::NONE_WORD),
            );
        } else if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::REASONING,
                &crate::codec::diagnostics::IR_DROP_REASONING,
                [],
                "omitting reasoning OFF on OpenAI Chat egress: reasoning_effort \"none\" is not \
                 accepted by every OpenAI reasoning model and this lane does not declare it"
            );
        } else if let Some(ask) = req.reasoning {
            let table = req
                .reasoning_budgets
                .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
            match ask.to_effort(table) {
                Some(effort) => {
                    out.insert(
                        REASONING_EFFORT.to_string(),
                        serde_json::json!(effort.as_three_word_str()),
                    );
                }

                None => crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "dropping a \"model decides\" reasoning ask on OpenAI Chat egress: \
                     reasoning_effort has no dynamic form"
                ),
            }
        }

        if let Some(top_logprobs) = req.top_logprobs {
            out.insert(keys::LOGPROBS.to_string(), serde_json::json!(true));
            out.insert(
                keys::TOP_LOGPROBS.to_string(),
                serde_json::json!(top_logprobs),
            );
        } else if let Some(logprobs) = req.logprobs {
            out.insert(keys::LOGPROBS.to_string(), serde_json::json!(logprobs));
        }
        if let Some(response_format) = &req.response_format {
            out.insert(
                keys::RESPONSE_FORMAT.to_string(),
                write_openai_response_format(response_format),
            );
        }

        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));

        let custom_tools = super::slots::write_custom_tools(req);
        if (!req.tools.is_empty() || !custom_tools.is_empty()) && !req.extra.contains_key(FUNCTIONS)
        {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in &req.tools {
                if let Some(hosted) = &tool.hosted {
                    if hosted.get(keys::TYPE).and_then(|t| t.as_str()) == Some(TOOL_TYPE_CUSTOM) {
                        tools_arr.push(hosted.clone());
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::TOOLS,
                            &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
                            [],
                            "dropping a hosted tool on OpenAI Chat egress: it has no Chat \
                             Completions tool shape"
                        );
                    }
                    continue;
                }
                let mut function_obj = serde_json::Map::new();
                function_obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));

                if let Some(desc) = &tool.description {
                    function_obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }

                let params = if !tool.input_schema.is_null() {
                    tool.input_schema.clone()
                } else {
                    serde_json::json!({})
                };
                function_obj.insert(keys::PARAMETERS.to_string(), params);

                if let Some(strict) = tool.strict {
                    function_obj.insert(keys::STRICT.to_string(), serde_json::json!(strict));
                }

                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(TOOL_TYPE_FUNCTION),
                );
                tool_obj.insert(
                    keys::FUNCTION.to_string(),
                    serde_json::Value::Object(function_obj),
                );

                tools_arr.push(serde_json::Value::Object(tool_obj));
            }
            tools_arr.extend(custom_tools.iter().cloned());
            if !tools_arr.is_empty() {
                out.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
            }
        }

        if let Some(v) = super::slots::write_tool_choice(req)
            .filter(|_| !req.extra.contains_key(keys::FUNCTION_CALL))
        {
            if req.tools.is_empty() && custom_tools.is_empty() {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on OpenAI egress: \"tool_choice\" is only allowed when \
                     \"tools\" are specified (likely because the hosted tools that carried it were \
                     stripped on the cross-protocol seam)"
                );
            } else {
                out.insert(keys::TOOL_CHOICE.to_string(), v);
            }
        }

        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );

        for (key, value) in &req.extra {
            if key == MAX_COMPLETION_TOKENS_SENTINEL
                || key == MESSAGE_NAMES_SENTINEL
                || key == MESSAGE_EXTRAS_SENTINEL
            {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        serde_json::Value::Object(out)
    }

    fn write_response_event(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match self.try_write_response_events(host, ev) {
            Ok(frames) => frames.into_iter().next(),
            Err(_) => Some(("".into(), self.encoding_failure(host))),
        }
    }

    fn try_write_response_events(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        ev: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, IrError> {
        let frame = match ev {
            IrStreamEvent::MessageStart {
                role,
                id,
                created,
                model,
                ..
            } => {
                let openai_role = match role {
                    crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                    crate::codec::ir::IrRole::User
                    | crate::codec::ir::IrRole::System
                    | crate::codec::ir::IrRole::Tool => return Ok(Vec::new()),
                };
                let delta_obj = serde_json::json!({ (keys::ROLE): openai_role });

                let chunk_id = self.carried_chunk_id(|| {
                    id.clone()
                        .map(Ok)
                        .unwrap_or_else(|| synth_completion_id(host))
                })?;
                let chunk_created = created.unwrap_or_else(|| host_now(host));

                let chunk_model = model_or_default(model.as_deref());
                let chunk = serde_json::json!({
                    (keys::ID): chunk_id,
                    (keys::OBJECT): OBJ_CHUNK,
                    (CREATED): chunk_created,
                    (keys::MODEL): chunk_model,
                    (CHOICES): [{
                        (keys::INDEX): 0,
                        (keys::DELTA): delta_obj,
                        (keys::FINISH_REASON): null
                    }]
                });
                Some(("".to_string(), chunk))
            }
            IrStreamEvent::BlockStart {
                index,
                block,
                refusal,
            } => match block {
                crate::codec::ir::IrBlockMeta::Text => {
                    if *refusal {
                        self.mark_refusal_block(*index);
                    }
                    None
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    let delta_obj = serde_json::json!({
                        (keys::TOOL_CALLS): [{
                            (keys::INDEX): index,
                            (keys::ID): id,
                            (keys::TYPE): TOOL_TYPE_FUNCTION,
                            (keys::FUNCTION): { (keys::NAME): name, (keys::ARGUMENTS): "" }
                        }]
                    });
                    delta_chunk(delta_obj)
                }

                crate::codec::ir::IrBlockMeta::Thinking { .. }
                | crate::codec::ir::IrBlockMeta::RedactedThinking
                | crate::codec::ir::IrBlockMeta::Image => None,
            },
            IrStreamEvent::BlockDelta { index, delta } => match delta {
                crate::codec::ir::IrDelta::TextDelta(text) => {
                    let delta_obj = if self.is_refusal_block(*index) {
                        serde_json::json!({ (keys::REFUSAL): text })
                    } else {
                        serde_json::json!({ (keys::CONTENT): text })
                    };
                    delta_chunk(delta_obj)
                }
                crate::codec::ir::IrDelta::InputJsonDelta(json) => {
                    let delta_obj = serde_json::json!({
                        (keys::TOOL_CALLS): [{
                            (keys::INDEX): index,
                            (keys::FUNCTION): { (keys::ARGUMENTS): json }
                        }]
                    });
                    delta_chunk(delta_obj)
                }
                crate::codec::ir::IrDelta::ThinkingDelta(_) => None,

                crate::codec::ir::IrDelta::SignatureDelta(_)
                | crate::codec::ir::IrDelta::RedactedReasoningDelta(_) => None,
                crate::codec::ir::IrDelta::MediaDelta(block) => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::block(block.kind_name()),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "dropping generated media on OpenAI Chat stream egress: no native media delta"
                    );
                    None
                }
                crate::codec::ir::IrDelta::CitationsDelta(cits) => {
                    let annotations =
                        super::super::url_citation_wire::chat_url_annotations("", 0, cits);
                    if annotations.is_empty() {
                        return Ok(Vec::new());
                    }
                    delta_chunk(serde_json::json!({ (keys::ANNOTATIONS): annotations }))
                }
                crate::codec::ir::IrDelta::LogprobsDelta(lps) => {
                    if lps.is_empty() {
                        None
                    } else {
                        let chunk_obj = serde_json::json!({
                            (keys::OBJECT): OBJ_CHUNK,
                            (CHOICES): [{
                                (keys::INDEX): 0,
                                (keys::DELTA): {},
                                (keys::LOGPROBS): crate::codec::logprob_wire::write_token_logprobs(lps),
                                (keys::FINISH_REASON): null
                            }]
                        });
                        Some(("".to_string(), chunk_obj))
                    }
                }
            },
            IrStreamEvent::BlockStop { .. } => None,
            IrStreamEvent::MessageDelta {
                stop_reason, usage, ..
            } => {
                let finish_reason: serde_json::Value = match stop_reason {
                    Some(r) => serde_json::json!(write_openai_stop_reason(*r)),
                    None => serde_json::Value::Null,
                };
                let delta_obj = serde_json::json!({});
                let mut chunk_obj = serde_json::json!({
                    (keys::OBJECT): OBJ_CHUNK,
                    (CHOICES): [{
                        (keys::INDEX): 0,
                        (keys::DELTA): delta_obj,
                        (keys::FINISH_REASON): finish_reason
                    }]
                });

                let cache_read = usage.cache_read_input_tokens.unwrap_or(0);

                let prompt_tokens = usage
                    .input_tokens
                    .saturating_add(cache_read)
                    .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
                let completion_tokens = usage.output_tokens;
                if prompt_tokens != 0 || completion_tokens != 0 {
                    if let Some(obj) = chunk_obj.as_object_mut() {
                        let mut usage_obj = serde_json::json!({
                            (PROMPT_TOKENS): prompt_tokens,
                            (COMPLETION_TOKENS): completion_tokens,
                            (keys::TOTAL_TOKENS): prompt_tokens.saturating_add(completion_tokens),
                        });
                        if usage.cache_read_input_tokens.is_some() {
                            if let Some(uo) = usage_obj.as_object_mut() {
                                uo.insert(
                                    PROMPT_TOKENS_DETAILS.to_string(),
                                    serde_json::json!({ (keys::CACHED_TOKENS): cache_read }),
                                );
                            }
                        }

                        if let Some(rt) = usage.detail.reasoning_tokens {
                            if let Some(uo) = usage_obj.as_object_mut() {
                                uo.insert(
                                    COMPLETION_TOKENS_DETAILS.to_string(),
                                    serde_json::json!({ (keys::REASONING_TOKENS): rt }),
                                );
                            }
                        }
                        obj.insert(keys::USAGE.to_string(), usage_obj);
                    }
                }

                if let Some(tier) =
                    usage.detail.service_tier.as_deref().and_then(|t| {
                        crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t)
                    })
                {
                    if let Some(obj) = chunk_obj.as_object_mut() {
                        obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
                    }
                }
                Some(("".to_string(), chunk_obj))
            }
            IrStreamEvent::MessageStop => None,
            IrStreamEvent::Error(err) => {
                let message = err
                    .provider_signal
                    .clone()
                    .unwrap_or_else(|| keys::ERROR_WORD.to_string());

                let error_type = match err.class {
                    busbar_contract::upstream::StatusClass::RateLimit => ERR_TYPE_RATE_LIMIT,
                    busbar_contract::upstream::StatusClass::Auth => ERR_TYPE_AUTHENTICATION,

                    busbar_contract::upstream::StatusClass::Billing => ERR_TYPE_INSUFFICIENT_QUOTA,
                    busbar_contract::upstream::StatusClass::ContextLength
                    | busbar_contract::upstream::StatusClass::ClientError => {
                        ERR_TYPE_INVALID_REQUEST
                    }
                    busbar_contract::upstream::StatusClass::Overloaded
                    | busbar_contract::upstream::StatusClass::ServerError
                    | busbar_contract::upstream::StatusClass::Timeout
                    | busbar_contract::upstream::StatusClass::Network => ERR_TYPE_SERVER_ERROR,
                };

                let error_obj = serde_json::json!({
                    (keys::ERROR_WORD): {
                        (keys::MESSAGE): message,
                        (keys::TYPE): error_type,
                        (keys::CODE): bearer_error_code(error_type),
                        (keys::PARAM): serde_json::Value::Null,
                    }
                });
                Some(("".to_string(), error_obj))
            }
        };
        Ok(frame.into_iter().collect())
    }

    fn new_stream_framing(&self) -> Box<dyn super::StreamFraming> {
        Box::<OpenAiStreamFraming>::default()
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }

    /// Native error envelope, served as `application/json`:
    fn write_error(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        let error_type = match kind {
            ERR_TYPE_INVALID_REQUEST | "invalid_request" | "bad_request" => {
                ERR_TYPE_INVALID_REQUEST
            }
            ERR_TYPE_AUTHENTICATION | "unauthorized" | keys::AUTH_WORD => ERR_TYPE_AUTHENTICATION,
            ERR_TYPE_PERMISSION | "permission_denied" | "forbidden" => ERR_TYPE_PERMISSION,
            ERR_TYPE_NOT_FOUND => ERR_TYPE_NOT_FOUND,
            ERR_TYPE_RATE_LIMIT | "rate_limit" | "too_many_requests" => ERR_TYPE_RATE_LIMIT,
            ERR_TYPE_SERVER_ERROR | "internal_error" | "internal_server_error" => {
                ERR_TYPE_SERVER_ERROR
            }
            crate::codec::protocol::KIND_API_ERROR => crate::codec::protocol::KIND_API_ERROR,

            ERR_TYPE_INSUFFICIENT_QUOTA => ERR_TYPE_INSUFFICIENT_QUOTA,

            crate::codec::protocol::KIND_OVERLOADED
            | ERR_TYPE_OVERLOADED
            | "service_unavailable"
            | "unavailable"
            | "transient"
            | "timeout"
            | "network"
            | "5xx" => ERR_TYPE_SERVER_ERROR,
            crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH => ERR_TYPE_INVALID_REQUEST,

            "" => {
                if (500..600).contains(&status) {
                    ERR_TYPE_SERVER_ERROR
                } else {
                    ERR_TYPE_INVALID_REQUEST
                }
            }

            other => other,
        };

        serde_json::json!({
            (keys::ERROR_WORD): {
                (keys::MESSAGE): message,
                (keys::TYPE): error_type,
                (keys::PARAM): serde_json::Value::Null,
                (keys::CODE): bearer_error_code(error_type),
            }
        })
    }

    fn write_response(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        self.try_write_response(host, resp)
            .unwrap_or_else(|_| self.encoding_failure(host))
    }

    fn try_write_response(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> Result<serde_json::Value, IrError> {
        let mut obj = serde_json::Map::new();

        let text_parts: Vec<&str> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                crate::codec::ir::IrBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();
        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            if let crate::codec::ir::IrBlock::ToolUse {
                id, name, input, ..
            } = block
            {
                let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                tool_calls_arr.push(serde_json::json!({
                    (keys::TYPE): TOOL_TYPE_FUNCTION,
                    (keys::ID): id,
                    (keys::FUNCTION): {
                        (keys::NAME): name,
                        (keys::ARGUMENTS): args_str
                    }
                }));
            }
            if matches!(
                block,
                crate::codec::ir::IrBlock::Image { .. }
                    | crate::codec::ir::IrBlock::Media { .. }
                    | crate::codec::ir::IrBlock::Json(_)
                    | crate::codec::ir::IrBlock::HostedToolRecord { .. }
                    | crate::codec::ir::IrBlock::ToolResult { .. }
            ) {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::block(block.kind_name()),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping an answer block on OpenAI Chat response egress: no native message member"
                );
            }
        }
        if resp.audio.is_some() {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("choices[].message.audio"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping native audio output while re-encoding a Chat response: this writer has no audio projection"
            );
        }

        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            if let crate::codec::ir::IrBlock::Thinking { text, redacted, .. } = block {
                if !text.is_empty() {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::THINKING,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [redacted = *redacted, byte_len = text.len(),],
                        "dropping a reasoning/thinking block on OpenAI Chat egress: the completion \
                         response shape has no thinking output field (lossy-by-target); the \
                         chain-of-thought is NOT forwarded on this cross-protocol response"
                    );
                }
            }
        }

        let flagged = resp
            .content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Text { refusal: true, .. }));
        let (content_val, refusal_val) = if flagged {
            let part = |want: bool| -> serde_json::Value {
                let texts: Vec<&str> = resp
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::codec::ir::IrBlock::Text { text, refusal, .. }
                            if *refusal == want =>
                        {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(texts.concat())
                }
            };
            (part(false), part(true))
        } else {
            let refused = resp.stop_reason == Some(crate::codec::ir::IrStopReason::Refusal);
            match (!text_parts.is_empty()).then(|| text_parts.concat()) {
                Some(text) if refused => (serde_json::Value::Null, serde_json::json!(text)),
                Some(text) => (serde_json::json!(text), serde_json::Value::Null),
                None => (serde_json::Value::Null, serde_json::Value::Null),
            }
        };
        let mut message_obj = serde_json::json!({
            (keys::ROLE): keys::ASSISTANT,
            (keys::CONTENT): content_val,
            (keys::REFUSAL): refusal_val,
        });

        if !tool_calls_arr.is_empty() {
            message_obj[keys::TOOL_CALLS] = serde_json::Value::Array(tool_calls_arr);
        }

        let mut base = 0usize;
        let mut annotations: Vec<serde_json::Value> = Vec::new();
        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            if let crate::codec::ir::IrBlock::Text {
                text,
                citations,
                refusal,
                ..
            } = block
            {
                if *refusal
                    || (!flagged
                        && resp.stop_reason == Some(crate::codec::ir::IrStopReason::Refusal))
                {
                    continue;
                }
                annotations.extend(super::super::url_citation_wire::chat_url_annotations(
                    text, base, citations,
                ));

                base += text.chars().count();
            }
        }
        if !annotations.is_empty() {
            message_obj[keys::ANNOTATIONS] = serde_json::Value::Array(annotations);
        }

        let mut choices_array: Vec<serde_json::Value> = Vec::new();

        let finish_reason: serde_json::Value = match resp.stop_reason {
            Some(r) => serde_json::json!(write_openai_stop_reason(r)),
            None => serde_json::json!(FINISH_STOP),
        };

        let mut choice_obj = serde_json::Map::new();
        choice_obj.insert(keys::INDEX.to_string(), serde_json::json!(0));
        choice_obj.insert(keys::MESSAGE.to_string(), message_obj);

        choice_obj.insert(
            keys::LOGPROBS.to_string(),
            if resp.logprobs.is_empty() {
                serde_json::Value::Null
            } else {
                crate::codec::logprob_wire::write_token_logprobs(&resp.logprobs)
            },
        );
        choice_obj.insert(keys::FINISH_REASON.to_string(), finish_reason);
        choices_array.push(serde_json::Value::Object(choice_obj));

        let id = resp
            .id
            .clone()
            .map(Ok)
            .unwrap_or_else(|| synth_completion_id(host))?;
        obj.insert(keys::ID.to_string(), serde_json::json!(id));
        obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_COMPLETION));
        let created = resp.created.unwrap_or_else(|| host_now(host));
        obj.insert(CREATED.to_string(), serde_json::json!(created));

        obj.insert(
            keys::MODEL.to_string(),
            serde_json::json!(model_or_default(resp.model.as_deref())),
        );

        if let Some(ref fp) = resp.system_fingerprint {
            obj.insert(SYSTEM_FINGERPRINT.to_string(), serde_json::json!(fp));
        }
        obj.insert(CHOICES.to_string(), serde_json::Value::Array(choices_array));

        let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);

        let prompt_tokens = resp
            .usage
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(resp.usage.cache_creation_input_tokens.unwrap_or(0));
        let mut usage_map = serde_json::Map::new();
        usage_map.insert(PROMPT_TOKENS.to_string(), serde_json::json!(prompt_tokens));
        usage_map.insert(
            COMPLETION_TOKENS.to_string(),
            serde_json::json!(resp.usage.output_tokens),
        );
        usage_map.insert(
            keys::TOTAL_TOKENS.to_string(),
            serde_json::json!(prompt_tokens.saturating_add(resp.usage.output_tokens)),
        );

        {
            let mut ptd = serde_json::Map::new();
            if resp.usage.cache_read_input_tokens.is_some() {
                ptd.insert(
                    keys::CACHED_TOKENS.to_string(),
                    serde_json::json!(cache_read),
                );
            }
            let modality = resp.usage.detail.by_modality.as_ref();
            if let Some(a) = resp
                .usage
                .detail
                .input_audio_tokens
                .or(modality.and_then(|m| m.input.audio))
            {
                ptd.insert(AUDIO_TOKENS.to_string(), serde_json::json!(a));
            }
            if let Some(t) = modality.and_then(|m| m.input.text) {
                ptd.insert(TEXT_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(i) = modality.and_then(|m| m.input.image) {
                ptd.insert(IMAGE_TOKENS.to_string(), serde_json::json!(i));
            }
            if !ptd.is_empty() {
                usage_map.insert(
                    PROMPT_TOKENS_DETAILS.to_string(),
                    serde_json::Value::Object(ptd),
                );
            }
        }

        {
            let mut ctd = serde_json::Map::new();
            if let Some(rt) = resp.usage.detail.reasoning_tokens {
                ctd.insert(keys::REASONING_TOKENS.to_string(), serde_json::json!(rt));
            }
            let modality = resp.usage.detail.by_modality.as_ref();
            if let Some(a) = resp
                .usage
                .detail
                .output_audio_tokens
                .or(modality.and_then(|m| m.output.audio))
            {
                ctd.insert(AUDIO_TOKENS.to_string(), serde_json::json!(a));
            }
            if let Some(t) = modality.and_then(|m| m.output.text) {
                ctd.insert(TEXT_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(t) = resp.usage.detail.accepted_prediction_tokens {
                ctd.insert(ACCEPTED_PREDICTION_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(t) = resp.usage.detail.rejected_prediction_tokens {
                ctd.insert(REJECTED_PREDICTION_TOKENS.to_string(), serde_json::json!(t));
            }
            if !ctd.is_empty() {
                usage_map.insert(
                    COMPLETION_TOKENS_DETAILS.to_string(),
                    serde_json::Value::Object(ctd),
                );
            }
        }
        obj.insert(
            keys::USAGE.to_string(),
            serde_json::Value::Object(usage_map),
        );

        if let Some(tier) = resp
            .usage
            .detail
            .service_tier
            .as_deref()
            .and_then(|t| crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t))
        {
            obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
        }

        Ok(serde_json::Value::Object(obj))
    }
}
