use super::*;
use crate::codec::keys;

#[cfg(test)]
#[path = "tests/remaining_gemini_tests.rs"]
mod remaining_gemini_tests;

fn tool_overflow_error() -> busbar_contract::protocol::IrError {
    busbar_contract::protocol::IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("tool arguments exceed the translation limit".to_string()),
        retry_after: None,
    }
}

/// One streamed `GenerateContentResponse` frame carrying `part` as the model's only part.
fn model_part_frame(part: serde_json::Value) -> (String, serde_json::Value) {
    (
        String::new(),
        serde_json::json!({
            (FIELD_CANDIDATES): [{
                (keys::CONTENT): { (keys::ROLE): keys::MODEL, (FIELD_PARTS): [part] }
            }]
        }),
    )
}

impl ProtocolWriter for GeminiWriter {
    /// Latch the stream's translation cap, the one its plane's settings carry.
    fn set_translate_cap(&self, cap: usize) {
        self.stream.lock().translate_cap = cap;
    }

    /// No image-fidelity slot: an image's `detail` is dropped, with the seam's warn.
    fn carries_image_detail(&self) -> bool {
        false
    }

    /// The controls Gemini has no form for, derived from the mapping file; `write_request` drops
    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
            .map(crate::codec::carry::Slot::name)
            .collect()
    }

    fn upstream_path(&self) -> &str {
        GEMINI_PATH_BASE
    }

    /// Gemini expresses the candidate count as `generationConfig.candidateCount` (the snake_case
    fn requested_candidate_count(&self, body: &serde_json::Value) -> Option<u64> {
        body.pointer("/generationConfig/candidateCount")
            .or_else(|| body.pointer("/generationConfig/candidate_count"))
            .and_then(|v| v.as_u64())
            .filter(|&k| k > 1)
    }

    /// Gemini's URL embeds the model AND the stream mode. Streaming requests go to
    fn upstream_path_for_stream(&self, model: &str, stream: bool) -> String {
        if stream {
            let (key, value) = super::STREAM_QUERY;
            format!("{GEMINI_PATH_BASE}/{model}:streamGenerateContent?{key}={value}")
        } else {
            format!("{GEMINI_PATH_BASE}/{model}:generateContent")
        }
    }

    fn upstream_path_for(&self, model: &str) -> String {
        format!("{GEMINI_PATH_BASE}/{model}:generateContent")
    }

    /// Gemini carries turns in `contents` as `{role, parts: [{text}]}`, and spells the assistant
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> bool {
        if !obj
            .get(FIELD_CONTENTS)
            .is_some_and(serde_json::Value::is_array)
        {
            return false;
        }
        let Some(pairs) = crate::codec::dialect::rewrite_text_pairs(messages) else {
            return false;
        };
        let framed: Vec<serde_json::Value> = pairs
            .into_iter()
            .map(|(role, text)| {
                let g_role = if role == "assistant" || role == keys::MODEL {
                    keys::MODEL
                } else {
                    keys::USER
                };
                serde_json::json!({ (keys::ROLE): g_role, (FIELD_PARTS): [{ (keys::TEXT): text }] })
            })
            .collect();
        obj.insert(FIELD_CONTENTS.to_string(), serde_json::Value::Array(framed));
        true
    }

    /// The production write: the lane model is known, so a reasoning-OFF ask is written only where
    fn write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        model: &str,
        _caps: &crate::codec::egress_prep::LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();

        if !req.system.is_empty() {
            let parts: Vec<_> = req
                .system
                .iter()
                .enumerate()
                .filter_map(|(block_index, block)| {
                    let _block_origin =
                        crate::codec::drops::block_scope(req.block_origins.system.get(block_index));
                    match block {
                        crate::codec::ir::IrBlock::Text { text, .. } => {
                            Some(serde_json::json!({ (keys::TEXT): text }))
                        }

                        _ => {
                            crate::codec::drops::writer_drop!(
                            crate::codec::drops::block(block.kind_name()),
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping non-text system block on Gemini egress: systemInstruction \
                             carries text only"
                        );
                            None
                        }
                    }
                })
                .collect();
            if !parts.is_empty() {
                out.insert(
                    FIELD_SYSTEM_INSTRUCTION.to_string(),
                    serde_json::json!({ (FIELD_PARTS): parts }),
                );
            }
        }

        let mut tool_name_by_id: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::new();
        for (message_index, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(message_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            for (block_index, block) in msg.content.iter().enumerate() {
                let _block_origin =
                    crate::codec::drops::block_scope(_message_origins.get(block_index));

                if let crate::codec::ir::IrBlock::ToolUse { id, name, .. } = block {
                    if !id.is_empty() {
                        tool_name_by_id.insert(id.as_str(), name.as_str());
                    }
                }
            }
        }

        let mut contents_arr: Vec<serde_json::Value> = Vec::new();
        for (message_index, msg) in req.messages.iter().enumerate() {
            let _message_origins = req
                .block_origins
                .messages
                .get(message_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let role_str = match msg.role {
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::MODEL,

                crate::codec::ir::IrRole::Tool => keys::USER,
                crate::codec::ir::IrRole::System => continue,
            };

            let mut parts_arr: Vec<serde_json::Value> = Vec::new();
            for (block_index, block) in msg.content.iter().enumerate() {
                let _block_origin =
                    crate::codec::drops::block_scope(_message_origins.get(block_index));

                match block {

                    b @ crate::codec::ir::IrBlock::Text { .. } if b.is_citation_carrier() => {}
                    crate::codec::ir::IrBlock::Text { text, .. } => {
                        parts_arr.push(serde_json::json!({ (keys::TEXT): text }))
                    }
                    crate::codec::ir::IrBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                        ..
                    } => {










                        parts_arr.push(function_call_part(
                            id,
                            name,
                            coerce_tool_args(input),
                            thought_signature.as_deref(),
                        ))
                    }
                    crate::codec::ir::IrBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {





                        let known_call = tool_name_by_id.get(tool_use_id.as_str()).copied();
                        let name: &str = known_call.unwrap_or(tool_use_id.as_str());
                        let response_text = content
                            .iter()
                            .filter_map(|b| match b {
                                crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");


                        let json_values: Vec<&serde_json::Value> = content
                            .iter()
                            .filter_map(|b| match b {
                                crate::codec::ir::IrBlock::Json(v) => Some(v),
                                _ => None,
                            })
                            .collect();


















                        let payload: serde_json::Value = if json_values.is_empty() {
                            crate::codec::json::parse_str(&response_text)
                                .unwrap_or_else(|_| serde_json::json!(response_text))
                        } else if response_text.is_empty() {
                            match json_values.as_slice() {
                                [one] => (*one).clone(),
                                many => serde_json::Value::Array(
                                    many.iter().map(|v| (*v).clone()).collect(),
                                ),
                            }
                        } else {
                            let mut all = vec![serde_json::json!(response_text)];
                            all.extend(json_values.iter().map(|v| (*v).clone()));
                            serde_json::Value::Array(all)
                        };


                        let response_val: serde_json::Value = if *is_error {
                            match payload {
                                serde_json::Value::Object(o) if o.contains_key(keys::ERROR_WORD) => {
                                    serde_json::Value::Object(o)
                                }
                                serde_json::Value::Null => serde_json::json!({ (keys::ERROR_WORD): {} }),
                                other => serde_json::json!({ (keys::ERROR_WORD): other }),
                            }
                        } else if payload.is_object() {
                            payload
                        } else if payload.is_null() {
                            serde_json::json!({})
                        } else {
                            serde_json::json!({ (keys::OUTPUT): payload })
                        };
                        let mut fr_obj = serde_json::Map::new();


                        if known_call.is_some() && !tool_use_id.is_empty() {
                            fr_obj.insert(keys::ID.to_string(), serde_json::json!(tool_use_id));
                        }
                        fr_obj.insert(keys::NAME.to_string(), serde_json::json!(name));
                        fr_obj.insert(keys::RESPONSE.to_string(), response_val);


                        let media_parts: Vec<serde_json::Value> =
                            content.iter().enumerate().filter_map(|(index, block)| crate::codec::drops::with_child_origin(index, || write_gemini_media_part(block))).collect();
                        if !media_parts.is_empty() {
                            fr_obj.insert(FIELD_PARTS.to_string(), serde_json::Value::Array(media_parts));
                        }
                        parts_arr.push(serde_json::json!({
                            (FIELD_FUNCTION_RESPONSE): serde_json::Value::Object(fr_obj)
                        }))
                    }
                    crate::codec::ir::IrBlock::Image { source, .. } => match source {







                        crate::codec::ir::IrImageSource::Url(uri) => parts_arr.push(serde_json::json!({
                            (FIELD_FILE_DATA): { (FIELD_FILE_URI): uri, (FIELD_MIME_TYPE): gemini_image_mime_for_url(uri) }
                        })),

                        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                            parts_arr.push(serde_json::json!({
                                (FIELD_INLINE_DATA): { (FIELD_MIME_TYPE): media_type, (keys::DATA): data }
                            }))
                        }


                        crate::codec::ir::IrImageSource::Vendor { .. } => {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::IMAGE,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [],
                                "dropping unresolvable vendor-scoped image reference on Gemini \
                                 egress: a file_id / s3Location has no cross-vendor analog");
                        }
                    },





                    crate::codec::ir::IrBlock::Media { kind, source, .. } => match source {
                        crate::codec::ir::IrImageSource::Url(uri) => {



                            parts_arr.push(serde_json::json!({
                                (FIELD_FILE_DATA): { (FIELD_FILE_URI): uri, (FIELD_MIME_TYPE): gemini_mime_for_kind(*kind) }
                            }))
                        }
                        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
                            parts_arr.push(serde_json::json!({
                                (FIELD_INLINE_DATA): { (FIELD_MIME_TYPE): media_type, (keys::DATA): data }
                            }))
                        }
                        crate::codec::ir::IrImageSource::Vendor { .. } => {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::block(kind.as_str()),
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [media_kind = kind.as_str(), ],
                                "dropping attachment on Gemini egress: the source is a vendor-scoped \
                                 file handle (an OpenAI/Anthropic file_id, a Bedrock s3Location) that \
                                 Gemini's backend cannot resolve; the block is NOT emitted");
                        }
                    },
                    crate::codec::ir::IrBlock::Json(_) | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {

                    }


                    crate::codec::ir::IrBlock::Thinking { redacted: true, .. } => {}
                    crate::codec::ir::IrBlock::Thinking {
                        text,
                        signature,
                        signature_origin,
                        ..
                    } => {






                        let mut part = serde_json::Map::new();
                        part.insert(keys::TEXT.to_string(), serde_json::json!(text));
                        part.insert(FIELD_THOUGHT.to_string(), serde_json::json!(true));
                        match (signature, signature_origin) {
                            (Some(sig), None | Some(crate::codec::ir::IrSignatureOrigin::Gemini)) => {
                                part.insert(FIELD_THOUGHT_SIGNATURE.to_string(), serde_json::json!(sig));
                            }
                            (Some(_), Some(origin)) => crate::codec::drops::writer_drop!(
                                crate::codec::drops::THINKING,
                                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                [?origin, ],
                                "dropping a foreign reasoning signature on Gemini egress: only a \
                                 Gemini-minted thoughtSignature is valid there"),
                            (None, _) => {}
                        }
                        parts_arr.push(serde_json::Value::Object(part));
                    }
                }
            }

            if parts_arr.is_empty() {
                parts_arr.push(serde_json::json!({ (keys::TEXT): "" }));
            }
            let mut content_obj = serde_json::Map::new();
            content_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));
            content_obj.insert(FIELD_PARTS.to_string(), serde_json::Value::Array(parts_arr));
            contents_arr.push(serde_json::Value::Object(content_obj));
        }

        if !contents_arr.is_empty() {
            out.insert(
                FIELD_CONTENTS.to_string(),
                serde_json::Value::Array(contents_arr),
            );
        }

        super::super::ir_encode::warn_dropped_tool_strict(&req.tools, COUNT_LABEL);

        let omit_to_subset = match (&req.allowed_tools, &req.tool_choice) {
            (Some(names), None | Some(crate::codec::ir::IrToolChoice::Auto)) => Some(names),
            _ => None,
        };
        let func_tools: Vec<&crate::codec::ir::IrTool> = req
            .tools
            .iter()
            .filter(|tool| omit_to_subset.is_none_or(|names| names.contains(&tool.name)))
            .collect();
        let hosted_entries = write_gemini_hosted_tools(&req.hosted_tools);
        if !func_tools.is_empty() || !hosted_entries.is_empty() {
            let func_decls: Vec<_> = func_tools
                .iter()
                .map(|tool| {
                    let mut obj = serde_json::Map::new();
                    obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));
                    if let Some(desc) = &tool.description {
                        obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                    }

                    obj.insert(
                        keys::PARAMETERS.to_string(),
                        sanitize_gemini_schema(&resolve_gemini_schema_refs(&tool.input_schema)),
                    );
                    serde_json::Value::Object(obj)
                })
                .collect();

            let mut tool_entries = Vec::with_capacity(1 + hosted_entries.len());
            if !func_decls.is_empty() {
                tool_entries.push(serde_json::json!({(FIELD_FUNCTION_DECLARATIONS): func_decls}));
            }
            tool_entries.extend(hosted_entries);
            out.insert(
                keys::TOOLS.to_string(),
                serde_json::Value::Array(tool_entries),
            );
        }

        let mut tool_config = req
            .extra
            .get(keys::TOOL_CONFIG)
            .and_then(|tc| tc.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(tc) = &req.tool_choice {
            if func_tools.is_empty() {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Gemini egress: a functionCallingConfig with no \
                     accompanying tools is meaningless (likely because the hosted tools that \
                     carried it were stripped on the cross-protocol seam)"
                );
            } else {
                let fcc = match (tc, &req.allowed_tools) {
                    (crate::codec::ir::IrToolChoice::Required, Some(names)) => {
                        serde_json::json!({(keys::MODE): GEMINI_ANY, (FIELD_ALLOWED_FUNCTION_NAMES): names})
                    }
                    _ => write_gemini_tool_choice(tc),
                };
                tool_config.insert(FIELD_FUNCTION_CALLING_CONFIG.to_string(), fcc);
            }
        }
        if !tool_config.is_empty() {
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
                "dropping parallel_tool_calls on Gemini egress: generateContent has no parallelism \
                 control, so the backend's default parallelism applies");
        }

        let mut gen_config = req
            .extra
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(max_tokens) = req.max_tokens {
            gen_config.insert(
                FIELD_MAX_OUTPUT_TOKENS.to_string(),
                serde_json::json!(max_tokens),
            );
        }

        out.insert(
            FIELD_GENERATION_CONFIG.to_string(),
            serde_json::Value::Object(gen_config),
        );
        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );
        let mut gen_config = match out.remove(FIELD_GENERATION_CONFIG) {
            Some(serde_json::Value::Object(gc)) => gc,
            _ => serde_json::Map::new(),
        };

        if req.top_logprobs.is_some() {
            gen_config.insert(FIELD_RESPONSE_LOGPROBS.to_string(), serde_json::json!(true));
        } else if let Some(logprobs) = req.logprobs {
            gen_config.insert(
                FIELD_RESPONSE_LOGPROBS.to_string(),
                serde_json::json!(logprobs),
            );
        }
        if let Some(top_logprobs) = req.top_logprobs {
            const GEMINI_MAX_TOP_LOGPROBS: u32 = 5;
            let clamped = top_logprobs.min(GEMINI_MAX_TOP_LOGPROBS);
            if clamped != top_logprobs {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOP_LOGPROBS,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [requested = top_logprobs, clamped,],
                    "clamping top_logprobs to Gemini's max (5)"
                );
            }

            if clamped >= 1 {
                gen_config.insert(keys::LOGPROBS.to_string(), serde_json::json!(clamped));
            }
        }

        if let Some(ask) = req.reasoning {
            let table = req
                .reasoning_budgets
                .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
            let budget: Option<i64> = match ask {
                crate::codec::ir::IrReasoningAsk::Dynamic => Some(-1),

                crate::codec::ir::IrReasoningAsk::Off => {
                    if gemini_model_accepts_thinking_off(model) {
                        Some(0)
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::REASONING,
                            &crate::codec::diagnostics::IR_DROP_REASONING,
                            [model,],
                            "dropping a reasoning-off ask on Gemini egress: thinkingBudget 0 is \
                             accepted only by the gemini-2.5-flash family"
                        );
                        None
                    }
                }
                other => other.to_budget(table).map(i64::from),
            };

            if let (Some(budget), false) = (budget, gen_config.contains_key(FIELD_THINKING_CONFIG))
            {
                let thinking_config = if matches!(ask, crate::codec::ir::IrReasoningAsk::Off) {
                    serde_json::json!({(FIELD_THINKING_BUDGET): budget})
                } else {
                    serde_json::json!({(FIELD_THINKING_BUDGET): budget, "includeThoughts": true})
                };
                gen_config.insert(FIELD_THINKING_CONFIG.to_string(), thinking_config);
            }
        }

        if let Some(rf) = &req.response_format {
            write_gemini_response_format(&mut gen_config, rf);
        }

        if let Some(modalities) = &req.output_modalities {
            if !gen_config.contains_key(FIELD_RESPONSE_MODALITIES) {
                gen_config.insert(
                    FIELD_RESPONSE_MODALITIES.to_string(),
                    write_gemini_response_modalities(modalities),
                );
            }
        }
        if !gen_config.is_empty() {
            out.insert(
                FIELD_GENERATION_CONFIG.to_string(),
                serde_json::Value::Object(gen_config),
            );
        }

        if let Some(metadata) = &req.metadata {
            out.insert(FIELD_LABELS.to_string(), write_gemini_labels(metadata));
        }

        crate::codec::carry::warn_drops(
            super::map::REQUEST,
            super::map::CONTROLS,
            Some(&super::map::DROP_WARN),
            req,
        );

        for (key, value) in &req.extra {
            if key == FIELD_GENERATION_CONFIG || key == keys::TOOL_CONFIG {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        serde_json::Value::Object(out)
    }

    /// Native Gemini error envelope: `{"error":{"code":<int>,"message":<msg>,"status":<UPPER_SNAKE>}}`.
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        fn status_name_for_http(status: u16) -> &'static str {
            if let Some((name, _)) = STATUS_HTTP.iter().find(|(_, code)| *code == status) {
                return name;
            }
            match status {
                409 => GEMINI_ABORTED,
                499 => GEMINI_CANCELLED,
                s if (400..500).contains(&s) => GRPC_INVALID_ARGUMENT,
                s if (500..600).contains(&s) => GRPC_INTERNAL,
                _ => GEMINI_UNKNOWN,
            }
        }

        fn status_name_for_kind(kind: &str) -> Option<&'static str> {
            match kind {
                ERR_TYPE_INVALID_REQUEST | "invalid_argument" | "bad_request" => {
                    Some(GRPC_INVALID_ARGUMENT)
                }
                ERR_TYPE_AUTHENTICATION | "unauthenticated" | keys::AUTH_WORD => {
                    Some(GRPC_UNAUTHENTICATED)
                }
                ERR_TYPE_PERMISSION | "permission_denied" | "forbidden" => {
                    Some(GRPC_PERMISSION_DENIED)
                }
                ERR_TYPE_NOT_FOUND | "not_found" => Some(GRPC_NOT_FOUND),
                ERR_TYPE_RATE_LIMIT | "resource_exhausted" | "rate_limit" => {
                    Some(GRPC_RESOURCE_EXHAUSTED)
                }
                ERR_TYPE_OVERLOADED
                | busbar_contract::protocol::KIND_OVERLOADED
                | "unavailable" => Some(GRPC_UNAVAILABLE),
                "deadline_exceeded" | crate::codec::framework::KIND_TIMEOUT => {
                    Some(GRPC_DEADLINE_EXCEEDED)
                }
                busbar_contract::protocol::KIND_API_ERROR
                | "internal"
                | crate::codec::framework::KIND_SERVER_ERROR => Some(GRPC_INTERNAL),
                "unimplemented" | "not_implemented" => Some(GRPC_UNIMPLEMENTED),
                _ => None,
            }
        }

        fn http_for_status_name(name: &str) -> Option<u16> {
            STATUS_HTTP
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, code)| *code)
        }

        let status_str = match status_name_for_kind(kind) {
            Some(name) if http_for_status_name(name) == Some(status) => name,
            _ => status_name_for_http(status),
        };

        let is_auth_bad_key = status == 400
            && status_str == GRPC_INVALID_ARGUMENT
            && message == GEMINI_BAD_KEY_MESSAGE;
        if is_auth_bad_key {
            serde_json::json!({
                (keys::ERROR_WORD): {
                    (keys::CODE): status,
                    (keys::MESSAGE): message,
                    (keys::STATUS): status_str,
                    (FIELD_DETAILS): [{
                        "@type": GEMINI_ERROR_INFO_TYPE_URL,
                        (keys::REASON): GEMINI_ERROR_REASON_API_KEY_INVALID,
                        (keys::DOMAIN): "googleapis.com",
                        "metadata": {
                            "service": "generativelanguage.googleapis.com"
                        }
                    }]
                }
            })
        } else {
            serde_json::json!({
                (keys::ERROR_WORD): {
                    (keys::CODE): status,
                    (keys::MESSAGE): message,
                    (keys::STATUS): status_str,
                }
            })
        }
    }

    fn try_write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        event: &IrStreamEvent,
    ) -> Result<Vec<(String, serde_json::Value)>, busbar_contract::protocol::IrError> {
        if let IrStreamEvent::BlockStop { index } = event {
            return self
                .flush_tool(*index)
                .map(|frame| frame.into_iter().collect());
        }
        Ok(self.write_response_event(host, event).into_iter().collect())
    }

    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match ev {
            IrStreamEvent::MessageStart { id, model, .. } => {
                let mut frame = serde_json::Map::new();

                let response_id =
                    self.carried_response_id(|| id.clone().or_else(|| synth_response_id(host)));
                if let Some(response_id) = response_id {
                    frame.insert(
                        FIELD_RESPONSE_ID.to_string(),
                        serde_json::json!(response_id),
                    );
                }

                if let Some(model) = model {
                    frame.insert(FIELD_MODEL_VERSION.to_string(), serde_json::json!(model));
                }
                Some(("".to_string(), serde_json::Value::Object(frame)))
            }

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => match block {
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    {
                        let guard = &mut self.stream.lock().open_tools;
                        let open_count = guard.len();
                        match guard.iter_mut().find(|t| t.index == *index) {
                            Some(entry) => {
                                entry.id = id.clone();
                                entry.name = name.clone();
                                entry.args.clear();
                                entry.overflowed = false;
                            }

                            None if open_count >= MAX_GEMINI_TOOL_FRAMES => {}
                            None => guard.push(GeminiOpenTool {
                                index: *index,
                                id: id.clone(),
                                name: name.clone(),
                                args: String::new(),
                                overflowed: false,
                            }),
                        }
                    }
                    None
                }

                crate::codec::ir::IrBlockMeta::Text
                | crate::codec::ir::IrBlockMeta::Thinking { .. }
                | crate::codec::ir::IrBlockMeta::RedactedThinking
                | crate::codec::ir::IrBlockMeta::Image => None,
            },

            IrStreamEvent::BlockDelta { index, delta } => match delta {
                crate::codec::ir::IrDelta::TextDelta(text) => {
                    if !text.is_empty() {
                        let mut stream = self.stream.lock();
                        let cap = stream.translate_cap;
                        let st = &mut stream.stream_text;
                        let emitted_start = st.emitted_bytes;
                        st.emitted_bytes = emitted_start.saturating_add(text.len());
                        let position = st
                            .block_starts
                            .iter()
                            .position(|block| block.index == *index);
                        if let Some(position) = position {
                            let block = &mut st.block_starts[position];
                            block.contiguous &= block.end == emitted_start;
                            block.end = st.emitted_bytes;
                        }
                        if !st.truncated && st.text.len().saturating_add(text.len()) <= cap {
                            let start = st.text.len();
                            if position.is_none() {
                                st.block_starts.push(GeminiStreamBlock {
                                    index: *index,
                                    start,
                                    end: st.emitted_bytes,
                                    contiguous: true,
                                });
                            }
                            extend_char_marks(&mut st.marks, &mut st.chars, start, text);
                            st.text.push_str(text);
                        } else {
                            st.truncated = true;
                        }
                    }
                    Some(model_part_frame(serde_json::json!({(keys::TEXT): text})))
                }

                crate::codec::ir::IrDelta::InputJsonDelta(json_str) => {
                    {
                        let mut stream = self.stream.lock();
                        let cap = stream.translate_cap;
                        if let Some(GeminiOpenTool {
                            args, overflowed, ..
                        }) = stream.open_tools.iter_mut().find(|t| t.index == *index)
                        {
                            if !*overflowed && args.len().saturating_add(json_str.len()) <= cap {
                                args.push_str(json_str);
                            } else {
                                *overflowed = true;
                            }
                        }
                    }
                    None
                }

                crate::codec::ir::IrDelta::ThinkingDelta(thinking) => Some(model_part_frame(
                    serde_json::json!({(keys::TEXT): thinking, (FIELD_THOUGHT): true}),
                )),

                crate::codec::ir::IrDelta::SignatureDelta(sig) => Some(model_part_frame(
                    serde_json::json!({(keys::TEXT): "", (FIELD_THOUGHT): true, (FIELD_THOUGHT_SIGNATURE): sig}),
                )),

                crate::codec::ir::IrDelta::RedactedReasoningDelta(_) => None,

                crate::codec::ir::IrDelta::MediaDelta(block) => {
                    super::write_gemini_media_part(block).map(model_part_frame)
                }

                crate::codec::ir::IrDelta::CitationsDelta(citations) => {
                    if citations.is_empty() {
                        None
                    } else {
                        let spans: Vec<_> =
                            {
                                let stream = self.stream.lock();
                                let st = &stream.stream_text;
                                let anchor = st
                                    .block_starts
                                    .iter()
                                    .find(|block| block.index == *index && block.contiguous);
                                match anchor {
                                    Some(block) => {
                                        let start = block.start;
                                        let end = block.end.min(st.text.len());
                                        let text = if st.truncated {
                                            ""
                                        } else {
                                            &st.text[start..end]
                                        };
                                        let idx = GeminiCharIndex::from_marks(&st.text, &st.marks);
                                        let base = idx.char_offset(start as i64);
                                        citations
                                            .iter()
                                            .map(|citation| {
                                                if native_citation_source(citation).is_some() {
                                                    return None;
                                                }
                                                crate::codec::url_citation_wire::citation_span(
                                                    text, 0, citation,
                                                )
                                                .map(|(s, e)| {
                                                    (
                                                        idx.byte_offset(s.saturating_add(base)),
                                                        idx.byte_offset(e.saturating_add(base)),
                                                    )
                                                })
                                            })
                                            .collect()
                                    }
                                    None => vec![None; citations.len()],
                                }
                            };
                        let sources: Vec<_> = citations
                            .iter()
                            .zip(spans)
                            .filter_map(|(citation, span)| write_gemini_citation(citation, span))
                            .collect();
                        if sources.is_empty() {
                            return None;
                        }
                        Some((
                            "".to_string(),
                            serde_json::json!({
                                (FIELD_CANDIDATES): [{
                                    (FIELD_CITATION_METADATA): { (FIELD_CITATION_SOURCES): sources }
                                }]
                            }),
                        ))
                    }
                }
                crate::codec::ir::IrDelta::LogprobsDelta(lps) => {
                    if lps.is_empty() {
                        None
                    } else {
                        Some((
                            "".to_string(),
                            serde_json::json!({
                                (FIELD_CANDIDATES): [{
                                    (FIELD_LOGPROBS_RESULT): write_gemini_logprobs_result(lps)
                                }]
                            }),
                        ))
                    }
                }
            },

            IrStreamEvent::BlockStop { index } => match self.flush_tool(*index) {
                Ok(frame) => frame,
                Err(error) => self.write_response_event(host, &IrStreamEvent::Error(error)),
            },

            IrStreamEvent::MessageDelta {
                stop_reason,
                usage,
                stop_sequence: _,
                stop_detail: _,
            } => {
                let (usage_metadata, _) = gemini_usage_metadata(usage, true);
                let mut out_obj = serde_json::Map::new();

                if let Some(reason) = stop_reason {
                    out_obj.insert(
                        FIELD_CANDIDATES.to_string(),
                        serde_json::json!([
                            { (FIELD_FINISH_REASON): write_gemini_stop_reason(*reason) }
                        ]),
                    );
                }
                out_obj.insert(
                    FIELD_USAGE_METADATA.to_string(),
                    serde_json::Value::Object(usage_metadata),
                );

                if let Some(create_time) = &usage.detail.create_time {
                    out_obj.insert(
                        FIELD_CREATE_TIME.to_string(),
                        serde_json::json!(create_time),
                    );
                }
                Some(("".to_string(), serde_json::Value::Object(out_obj)))
            }

            IrStreamEvent::MessageStop => None,

            IrStreamEvent::Error(err) => {
                let (code, status_name) = gemini_stream_error_code_status(err.class);
                let message = err
                    .provider_signal
                    .clone()
                    .unwrap_or_else(|| keys::ERROR_WORD.to_string());
                Some((
                    "".to_string(),
                    serde_json::json!({
                        (keys::ERROR_WORD): {
                            (keys::CODE): code,
                            (keys::MESSAGE): message,
                            (keys::STATUS): status_name,
                        }
                    }),
                ))
            }
        }
    }

    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        let mut parts_arr: Vec<serde_json::Value> = Vec::new();

        let mut citation_sources: Vec<serde_json::Value> = Vec::new();
        let mut byte_prefix: i64 = 0;

        for (block_index, block) in resp.content.iter().enumerate() {
            let _block_origin =
                crate::codec::drops::block_scope(resp.block_origins.response.get(block_index));

            match block {
                crate::codec::ir::IrBlock::Text {
                    text, citations, ..
                } => {
                    if !citations.is_empty() {
                        if text.is_empty() {
                            for c in citations {
                                citation_sources.extend(write_gemini_citation(c, None));
                            }
                        } else {
                            let idx = std::cell::OnceCell::new();
                            let convert = |v: i64| {
                                idx.get_or_init(|| GeminiCharIndex::build(text))
                                    .byte_offset(v)
                                    + byte_prefix
                            };
                            for c in citations {
                                let span = if native_citation_source(c).is_some() {
                                    None
                                } else {
                                    crate::codec::url_citation_wire::citation_span(text, 0, c)
                                        .map(|(start, end)| (convert(start), convert(end)))
                                };
                                citation_sources.extend(write_gemini_citation(c, span));
                            }
                        }
                    }
                    byte_prefix += text.len() as i64;
                    if !text.is_empty() {
                        parts_arr.push(serde_json::json!({(keys::TEXT): text}));
                    }
                }

                crate::codec::ir::IrBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                    ..
                } => {
                    parts_arr.push(function_call_part(
                        id,
                        name,
                        coerce_tool_args(input),
                        thought_signature.as_deref(),
                    ));
                }

                crate::codec::ir::IrBlock::Thinking { redacted: true, .. } => {}
                crate::codec::ir::IrBlock::Thinking {
                    text, signature, ..
                } => {
                    let mut part = serde_json::Map::new();
                    part.insert(keys::TEXT.to_string(), serde_json::json!(text));
                    part.insert(FIELD_THOUGHT.to_string(), serde_json::json!(true));
                    if let Some(sig) = signature {
                        part.insert(FIELD_THOUGHT_SIGNATURE.to_string(), serde_json::json!(sig));
                    }
                    parts_arr.push(serde_json::Value::Object(part));
                }

                crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. } => {
                    if let Some(part) = super::write_gemini_media_part(block) {
                        parts_arr.push(part);
                    }
                }
                crate::codec::ir::IrBlock::ToolResult { .. }
                | crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {}
            }
        }

        let finish_reason = resp
            .stop_reason
            .map(write_gemini_stop_reason)
            .unwrap_or(GEMINI_FINISH_STOP);

        let (mut usage_metadata, total) = gemini_usage_metadata(&resp.usage, false);
        if let Some(m) = &resp.usage.detail.by_modality {
            super::usage::write_by_modality(m, &mut usage_metadata);
        }
        if resp.created.is_some() || resp.model.is_some() {
            usage_metadata.insert(
                FIELD_TOTAL_TOKEN_COUNT.to_string(),
                serde_json::json!(total),
            );
        }
        let mut candidate = serde_json::json!({
            (keys::CONTENT): {
                (keys::ROLE): keys::MODEL,
                (FIELD_PARTS): parts_arr
            }
        });
        candidate[FIELD_FINISH_REASON] = serde_json::json!(finish_reason);

        if !citation_sources.is_empty() {
            candidate[FIELD_CITATION_METADATA] = serde_json::json!({
                (FIELD_CITATION_SOURCES): citation_sources
            });
        }

        if !resp.logprobs.is_empty() {
            candidate[FIELD_LOGPROBS_RESULT] = write_gemini_logprobs_result(&resp.logprobs);
        }

        if let Some(gm) = super::citations::write_grounding_chunks(&resp.content) {
            candidate[super::citations::FIELD_GROUNDING_METADATA] = gm;
        }
        if let Some(ratings) = super::write_safety_ratings(&resp.safety) {
            candidate[super::FIELD_SAFETY_RATINGS] = ratings;
        }
        let mut out = serde_json::json!({
            (FIELD_CANDIDATES): [candidate]
        });
        out[FIELD_USAGE_METADATA] = serde_json::Value::Object(usage_metadata);

        if let Some(ref model) = resp.model {
            out[FIELD_MODEL_VERSION] = serde_json::json!(model);
        }

        if let Some(ref create_time) = resp.usage.detail.create_time {
            out[FIELD_CREATE_TIME] = serde_json::json!(create_time);
        }

        match (&resp.id, resp.created) {
            (Some(id), _) => {
                out[FIELD_RESPONSE_ID] = serde_json::json!(id);
            }
            (None, Some(_)) => {
                if let Some(id) = synth_response_id(host) {
                    out[FIELD_RESPONSE_ID] = serde_json::json!(id);
                }
            }
            (None, None) => {}
        }
        out
    }

    fn make_array_stream_framer(
        &self,
    ) -> Option<Box<dyn busbar_contract::protocol::ArrayStreamFramer>> {
        Some(Box::new(GeminiJsonArrayFramer::new()))
    }

    fn wants_array_stream(&self, body: &serde_json::Value) -> bool {
        body.get(GEMINI_JSON_ARRAY_SHIM_KEY)
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
    }

    /// IR-18: a `thoughtSignature` is read back as Gemini-minted.
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        origin == crate::codec::ir::IrSignatureOrigin::Gemini
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }
}

/// One native `functionCall` part: `{functionCall: {id?, name, args}, thoughtSignature?}`. The id
fn function_call_part(
    id: &str,
    name: &str,
    args: serde_json::Value,
    thought_signature: Option<&str>,
) -> serde_json::Value {
    let mut fc_obj = serde_json::Map::new();
    if !id.is_empty() {
        fc_obj.insert(keys::ID.to_string(), serde_json::json!(id));
    }
    fc_obj.insert(keys::NAME.to_string(), serde_json::json!(name));
    fc_obj.insert(FIELD_ARGS.to_string(), args);
    let mut part_obj = serde_json::Map::new();
    part_obj.insert(
        FIELD_FUNCTION_CALL.to_string(),
        serde_json::Value::Object(fc_obj),
    );
    if let Some(sig) = thought_signature {
        part_obj.insert(FIELD_THOUGHT_SIGNATURE.to_string(), serde_json::json!(sig));
    }
    serde_json::Value::Object(part_obj)
}

/// The native `usageMetadata` RECONSTRUCTED from the normalized IR usage, shared by the stream's
fn gemini_usage_metadata(
    usage: &crate::codec::ir::IrUsage,
    emit_total: bool,
) -> (serde_json::Map<String, serde_json::Value>, u64) {
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let tool_use = usage.detail.tool_use_prompt_tokens.unwrap_or(0);
    let prompt_total = usage
        .input_tokens
        .saturating_sub(tool_use)
        .saturating_add(cache_read)
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
    let total = prompt_total
        .saturating_add(usage.output_tokens)
        .saturating_add(tool_use);
    let mut usage_metadata = serde_json::Map::new();
    usage_metadata.insert(
        FIELD_PROMPT_TOKEN_COUNT.to_string(),
        serde_json::json!(prompt_total),
    );
    usage_metadata.insert(
        FIELD_CANDIDATES_TOKEN_COUNT.to_string(),
        serde_json::json!(usage.output_tokens),
    );
    if emit_total {
        usage_metadata.insert(
            FIELD_TOTAL_TOKEN_COUNT.to_string(),
            serde_json::json!(total),
        );
    }
    if usage.cache_read_input_tokens.is_some() {
        usage_metadata.insert(
            FIELD_CACHED_CONTENT_TOKEN_COUNT.to_string(),
            serde_json::json!(cache_read),
        );
    }
    if let Some(tool_use_prompt) = usage.detail.tool_use_prompt_tokens {
        usage_metadata.insert(
            FIELD_TOOL_USE_PROMPT_TOKEN_COUNT.to_string(),
            serde_json::json!(tool_use_prompt),
        );
    }
    if let Some(traffic_type) = &usage.detail.traffic_type {
        usage_metadata.insert(
            FIELD_TRAFFIC_TYPE.to_string(),
            serde_json::json!(traffic_type),
        );
    }
    (usage_metadata, total)
}

impl GeminiWriter {
    fn flush_tool(&self, index: usize) -> Result<Option<(String, serde_json::Value)>, IrError> {
        let tool = {
            let mut stream = self.stream.lock();
            stream
                .open_tools
                .iter()
                .position(|tool| tool.index == index)
                .map(|position| stream.open_tools.remove(position))
        };
        let Some(tool) = tool else { return Ok(None) };
        if tool.overflowed {
            return Err(tool_overflow_error());
        }
        let args = if tool.args.is_empty() {
            serde_json::json!({})
        } else {
            crate::codec::json::parse_str::<serde_json::Value>(&tool.args).map_err(|_| IrError {
                class: StatusClass::ServerError,
                provider_signal: Some("malformed streamed tool arguments".to_string()),
                retry_after: None,
            })?
        };
        Ok(Some(model_part_frame(function_call_part(
            &tool.id,
            &tool.name,
            coerce_tool_args(&args),
            None,
        ))))
    }
}
