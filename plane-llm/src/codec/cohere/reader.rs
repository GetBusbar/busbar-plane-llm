use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::usage_count::{CountRead, CountSlot, UsageCount};

/// What a translate attempt cannot carry from this dialect (`codec::drops`).
static GRAMMAR: crate::codec::drops::Grammar = crate::codec::drops::Grammar {
    request_map: super::map::REQUEST,
    parked: &[],
    request_blocks: super::REQUEST_BLOCKS,
    response_blocks: super::RESPONSE_BLOCKS,
    response_carried: Some(crate::codec::drops::Carried {
        map: super::map::RESPONSE_PATHS,
        code: super::RESPONSE_CODE,
        drops: super::RESPONSE_DROPS,
        defaults: &[],
        reroot: None,
    }),
    stream_carried: Some(crate::codec::drops::Carried {
        map: super::map::STREAM_PATHS,
        code: super::STREAM_CODE,
        drops: super::STREAM_DROPS,
        defaults: &[],
        reroot: None,
    }),
    stream_keyed_by_event: false,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for CohereReader {
    /// The top-level `finish_reason`, which `read_response` reads its stop reason from.
    fn stop_reason_key(&self) -> Option<crate::codec::usage_tail::StopKey> {
        Some(crate::codec::usage_tail::StopKey {
            key: b"\"finish_reason\"",
            depth: 1,
        })
    }

    /// The same `read_cohere_stop_reason` mapping as `read_response`; an empty token is no reason.
    fn stop_reason_of_token(&self, token: &str) -> Option<crate::codec::ir::IrStopReason> {
        (!token.is_empty()).then(|| read_cohere_stop_reason(token))
    }

    fn recover_response_usage(
        &self,
        body: &serde_json::Value,
    ) -> Option<busbar_contract::billing::TokenUsage> {
        Some(
            read_cohere_usage(Some(body.get(keys::USAGE)?))
                .ok()?
                .to_token_usage(),
        )
    }

    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;

        Some(read_cohere_usage(Some(&v)).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        let json = crate::codec::json::parse::<serde_json::Value>(body).ok();
        let provider_code = json
            .as_ref()
            .and_then(|j| j.get(keys::MESSAGE))
            .and_then(|m| m.as_str())
            .map(String::from);
        let structured_type = json
            .as_ref()
            .and_then(|j| j.get("error_type"))
            .and_then(|e| e.as_str())
            .map(String::from);

        let provider_code = if (status == StatusCode::BAD_REQUEST
            || status == StatusCode::PAYLOAD_TOO_LARGE)
            && Self::body_signals_context_length(body)
        {
            Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
        } else {
            provider_code
        };

        busbar_contract::upstream::RawUpstreamError {
            http_status: status.as_u16(),
            provider_code,
            structured_type,
            retry_after_secs: None,
        }
    }

    #[cfg(test)]
    fn classify(&self, status: StatusCode, body: &[u8]) -> CanonicalSignal {
        crate::codec::dialect_classify::cohere(status, body)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;
        let mut citation_budget = super::CohereCitationBudget::default();

        let mut extra = serde_json::Map::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();

        let mut system_turns_folded: usize = 0;

        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            let msgs_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;

            for msg_val in msgs_arr {
                let role_str = msg_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let role = match role_str {
                    keys::SYSTEM => crate::codec::ir::IrRole::System,
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                    keys::TOOL => crate::codec::ir::IrRole::Tool,
                    _ => return Err(ir_parse_error()),
                };

                if let Some(cv) = msg_val.get(keys::CONTENT) {
                    if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                        return Err(ir_parse_error());
                    }
                }

                if role == crate::codec::ir::IrRole::System {
                    system_turns_folded += 1;
                    let blocks_before = system_blocks.len();
                    if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if let Some(s) = content_val.as_str() {
                            {
                                block_origins
                                    .system
                                    .push(crate::codec::ir::IrBlockOrigin::at(
                                        &SOURCE_MESSAGE_CONTENT,
                                    ));
                                system_blocks.push(crate::codec::ir::IrBlock::text(s.to_string()))
                            };
                        } else if let Some(arr) = content_val.as_array() {
                            for block_val in arr {
                                if let Some(bo) = block_val.as_object() {
                                    if bo.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(keys::TEXT)
                                    {
                                        if let Some(text) =
                                            bo.get(keys::TEXT).and_then(|t| t.as_str())
                                        {
                                            {
                                                block_origins.system.push(
                                                    crate::codec::ir::IrBlockOrigin::at(
                                                        &SOURCE_MESSAGE_TEXT,
                                                    ),
                                                );
                                                system_blocks.push(crate::codec::ir::IrBlock::text(
                                                    text.to_string(),
                                                ))
                                            };
                                        }
                                    } else {
                                        let block_type = bo
                                            .get(keys::TYPE)
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("<missing>");
                                        crate::codec::drops::writer_drop!(
                                            crate::codec::drops::wire("messages[].content[]"),
                                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                            [block_type,],
                                            "dropping non-text block in cohere system array (cohere \
                                             system is text-only)"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    crate::codec::ir::IrSystemFold::record(
                        &mut system_folds,
                        messages.len(),
                        crate::codec::ir::IrSystemRole::System,
                        blocks_before,
                        system_blocks.len(),
                    );
                    continue;
                }

                let mut msg_content = Vec::new();
                let mut msg_origins = Vec::new();

                if role != crate::codec::ir::IrRole::Tool {
                    if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if content_val.is_string() {
                            {
                                msg_origins.push(crate::codec::ir::IrBlockOrigin::at(
                                    &SOURCE_MESSAGE_CONTENT,
                                ));
                                msg_content.push(crate::codec::ir::IrBlock::text(
                                    content_val.as_str().unwrap_or("").to_string(),
                                ))
                            };
                        } else if let Some(arr) = content_val.as_array() {
                            for block_val in arr {
                                if let Some(block_obj) = block_val.as_object() {
                                    match block_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
                                        Some(keys::TEXT) => {
                                            if let Some(text) =
                                                block_obj.get(keys::TEXT).and_then(|t| t.as_str())
                                            {
                                                {
                                                    msg_origins.push(
                                                        crate::codec::ir::IrBlockOrigin::at(
                                                            &SOURCE_MESSAGE_TEXT,
                                                        ),
                                                    );
                                                    msg_content.push(
                                                        crate::codec::ir::IrBlock::text(
                                                            text.to_string(),
                                                        ),
                                                    )
                                                };
                                            }
                                        }

                                        Some(keys::IMAGE_URL) => {
                                            if let Some(url) = block_obj
                                                .get(keys::IMAGE_URL)
                                                .and_then(|iu| iu.get(keys::URL))
                                                .and_then(|u| u.as_str())
                                            {
                                                {
                                                    msg_origins.push(
                                                        crate::codec::ir::IrBlockOrigin::at(
                                                            &SOURCE_MESSAGE_IMAGE,
                                                        ),
                                                    );
                                                    msg_content.push(crate::codec::ir::IrBlock::Image {
                                                    source:
                                                        super::super::ir_encode::parse_image_url(
                                                            url,
                                                        ),
                                                    cache_control: None,

                                                    detail: read_cohere_image_detail(
                                                        block_obj.get(keys::IMAGE_URL),
                                                    ),
                                                })
                                                };
                                            }
                                        }

                                        Some(keys::DOCUMENT) => {
                                            if let Some(doc) = block_obj.get(keys::DOCUMENT) {
                                                {
                                                    msg_origins.push(
                                                        crate::codec::ir::IrBlockOrigin::at(
                                                            &SOURCE_MESSAGE_DOCUMENT,
                                                        ),
                                                    );
                                                    msg_content.push(read_cohere_document(doc))
                                                };
                                            }
                                        }

                                        Some(keys::THINKING)
                                            if role == crate::codec::ir::IrRole::Assistant =>
                                        {
                                            if let Some(text) = block_obj
                                                .get(keys::THINKING)
                                                .and_then(|t| t.as_str())
                                                .filter(|s| !s.is_empty())
                                            {
                                                {
                                                    msg_origins.push(
                                                        crate::codec::ir::IrBlockOrigin::at(
                                                            &SOURCE_MESSAGE_THINKING,
                                                        ),
                                                    );
                                                    msg_content.push(
                                                        crate::codec::ir::IrBlock::Thinking {
                                                            text: text.to_string(),
                                                            signature: None,
                                                            redacted: false,
                                                            cache_control: None,
                                                            kind: None,
                                                            signature_origin: None,
                                                        },
                                                    )
                                                };
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }

                if role == crate::codec::ir::IrRole::Assistant {
                    if let Some(plan) = msg_val.get(TOOL_PLAN).and_then(|p| p.as_str()) {
                        if !plan.is_empty() {
                            {
                                msg_origins.insert(
                                    0,
                                    crate::codec::ir::IrBlockOrigin::at(&SOURCE_MESSAGE_TOOL_PLAN),
                                );
                                msg_content.insert(
                                    0,
                                    crate::codec::ir::IrBlock::Thinking {
                                        text: plan.to_string(),
                                        signature: None,
                                        redacted: false,
                                        cache_control: None,
                                        kind: None,
                                        signature_origin: None,
                                    },
                                )
                            };
                        }
                    }
                }

                if role != crate::codec::ir::IrRole::Tool {
                    if let Some(cits_val) = msg_val.get(keys::CITATIONS) {
                        let cits = super::read_cohere_citations(cits_val);
                        if super::attach_cohere_citations(
                            &mut msg_content,
                            cits,
                            &mut citation_budget,
                        )? {
                            msg_origins.push(crate::codec::ir::IrBlockOrigin::at(
                                &SOURCE_MESSAGE_CITATIONS,
                            ));
                        }
                    }
                }

                if role == crate::codec::ir::IrRole::Assistant {
                    if let Some(tool_calls) = msg_val
                        .get(keys::TOOL_CALLS)
                        .filter(|value| !value.is_null())
                    {
                        let calls = tool_calls.as_array().ok_or_else(ir_parse_error)?;
                        for call in calls {
                            let function = call
                                .get(keys::FUNCTION)
                                .and_then(|v| v.as_object())
                                .ok_or_else(ir_parse_error)?;
                            let nonempty = |value: Option<&serde_json::Value>| {
                                value
                                    .and_then(|v| v.as_str())
                                    .filter(|value| !value.is_empty())
                                    .map(String::from)
                                    .ok_or_else(ir_parse_error)
                            };
                            let id = nonempty(call.get(keys::ID))?;
                            let name = nonempty(function.get(keys::NAME))?;
                            let input = match function.get(keys::ARGUMENTS) {
                                None | Some(serde_json::Value::Null) => serde_json::json!({}),
                                Some(value) => {
                                    let arguments = value.as_str().ok_or_else(ir_parse_error)?;
                                    crate::codec::json::parse_str(arguments)
                                        .map_err(|_| ir_parse_error())?
                                }
                            };
                            {
                                msg_origins.push(crate::codec::ir::IrBlockOrigin::at(
                                    &SOURCE_MESSAGE_TOOL_CALL,
                                ));
                                msg_content.push(crate::codec::ir::IrBlock::ToolUse {
                                    id,
                                    name,
                                    input,
                                    cache_control: None,
                                    thought_signature: None,
                                })
                            };
                        }
                    }
                }

                if role == crate::codec::ir::IrRole::Tool {
                    let tool_call_id = msg_val
                        .get(keys::TOOL_CALL_ID)
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                        .ok_or_else(ir_parse_error)?
                        .to_string();
                    let mut result_content = Vec::new();
                    let mut result_origins = Vec::new();
                    let mut content_text = String::new();
                    if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if let Some(arr) = content_val.as_array() {
                            for b in arr {
                                let text = if let Some(s) = b.as_str() {
                                    Some(s.to_owned())
                                } else if let Some(bo) = b.as_object() {
                                    if bo.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(keys::TEXT)
                                    {
                                        bo.get(keys::TEXT)
                                            .and_then(|t| t.as_str())
                                            .map(String::from)
                                    } else if bo.contains_key(keys::DOCUMENT) {
                                        None
                                    } else {
                                        crate::codec::json::to_string(b).ok()
                                    }
                                } else {
                                    crate::codec::json::to_string(b).ok()
                                };
                                if let Some(text) = text {
                                    content_text.push_str(&text);
                                }
                                if let Some(doc) = b.get(keys::DOCUMENT) {
                                    if !content_text.is_empty() {
                                        {
                                            result_origins.push(
                                                crate::codec::ir::IrBlockOrigin::at(
                                                    &SOURCE_MESSAGE_CONTENT,
                                                ),
                                            );
                                            result_content.push(crate::codec::ir::IrBlock::text(
                                                std::mem::take(&mut content_text),
                                            ))
                                        };
                                    }
                                    {
                                        result_origins.push(crate::codec::ir::IrBlockOrigin::at(
                                            &SOURCE_MESSAGE_DOCUMENT,
                                        ));
                                        result_content.push(read_cohere_tool_result_document(doc))
                                    };
                                }
                            }
                        } else if let Some(s) = content_val.as_str() {
                            content_text.push_str(s);
                        } else {
                            content_text =
                                crate::codec::json::to_string(content_val).unwrap_or_default();
                        }
                    }
                    if !content_text.is_empty() {
                        {
                            result_origins
                                .push(crate::codec::ir::IrBlockOrigin::at(&SOURCE_MESSAGE_CONTENT));
                            result_content.push(crate::codec::ir::IrBlock::text(content_text))
                        };
                    }

                    if result_content.is_empty() {
                        {
                            result_origins.push(if msg_val.get(keys::CONTENT).is_some() {
                                crate::codec::ir::IrBlockOrigin::at(&SOURCE_MESSAGE_CONTENT)
                            } else {
                                crate::codec::ir::IrBlockOrigin::default()
                            });
                            result_content.push(crate::codec::ir::IrBlock::text(String::new()))
                        };
                    }
                    {
                        msg_origins.push(crate::codec::ir::IrBlockOrigin {
                            source: msg_val.get(keys::CONTENT).map(|_| &SOURCE_MESSAGE_CONTENT),
                            children: result_origins,
                        });
                        msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                            tool_use_id: tool_call_id,
                            content: result_content,
                            is_error: false,
                            cache_control: None,
                        })
                    };
                }

                {
                    block_origins.messages.push(msg_origins);
                    messages.push(crate::codec::ir::IrMessage {
                        role,
                        content: msg_content,
                    })
                };
            }
        } else {
            return Err(ir_parse_error());
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        if let Some(tools_value) = obj.get(keys::TOOLS).filter(|value| !value.is_null()) {
            let tools_arr = tools_value.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                let func_obj = tool_val
                    .get(keys::FUNCTION)
                    .and_then(|v| v.as_object())
                    .ok_or_else(ir_parse_error)?;
                let name = func_obj
                    .get(keys::NAME)
                    .and_then(|v| v.as_str())
                    .filter(|name| !name.is_empty())
                    .ok_or_else(ir_parse_error)?
                    .to_string();
                let description = func_obj
                    .get(keys::DESCRIPTION)
                    .and_then(|v| v.as_str().map(String::from));
                let input_schema = func_obj
                    .get(keys::PARAMETERS)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                tools.push(crate::codec::ir::IrTool {
                    name,
                    description,
                    input_schema,
                    cache_control: None,
                    hosted: None,
                    strict: None,
                });
            }
        }

        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_u64()
                    .filter(|value| *value > 0)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(ir_parse_error)
            })
            .transpose()?;

        let tool_choice = read_cohere_tool_choice(obj.get(keys::TOOL_CHOICE));
        if obj
            .get(keys::TOOL_CHOICE)
            .is_some_and(|value| !value.is_null())
            && tool_choice.is_none()
        {
            tracing::warn!(
                field = "tool_choice",
                "unrecognized Cohere tool_choice uses the documented automatic tool policy"
            );
        }
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let response_format = obj
            .get(keys::RESPONSE_FORMAT)
            .and_then(read_cohere_response_format);

        let logprobs = obj.get(keys::LOGPROBS).and_then(|v| v.as_bool());

        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        if !tools.is_empty() {
            if let Some(strict) = obj.get(STRICT_TOOLS).and_then(|v| v.as_bool()) {
                for tool in &mut tools {
                    tool.strict = Some(strict);
                }
                extra.remove(STRICT_TOOLS);
            }
        }

        if let Some(docs) = obj.get(keys::DOCUMENTS).and_then(|d| d.as_array()) {
            let mut document_origins = Vec::new();
            let blocks: Vec<crate::codec::ir::IrBlock> = docs
                .iter()
                .map(|document| {
                    document_origins.push(crate::codec::ir::IrBlockOrigin::at(
                        &SOURCE_GROUNDING_DOCUMENT,
                    ));
                    read_cohere_document(document)
                })
                .collect();
            match messages
                .iter()
                .position(|m| m.role == crate::codec::ir::IrRole::User)
            {
                Some(first_user) => {
                    messages[first_user].content.splice(0..0, blocks);
                    block_origins.messages[first_user].splice(0..0, document_origins);
                }
                None if !blocks.is_empty() => {
                    block_origins.messages.insert(0, document_origins);
                    messages.insert(
                        0,
                        crate::codec::ir::IrMessage {
                            role: crate::codec::ir::IrRole::User,
                            content: blocks,
                        },
                    );
                }
                None => {}
            }
            extra.remove(keys::DOCUMENTS);
        }

        let reasoning = read_cohere_reasoning(obj.get(keys::THINKING));
        if reasoning.is_some() {
            extra.remove(keys::THINKING);
        }

        let mut ir = crate::codec::ir::IrRequest {
            block_origins,
            reasoning,

            logprobs,
            system: system_blocks,
            system_turns_folded,
            system_folds,
            messages,
            tools,
            max_tokens,
            tool_choice,
            stream,
            response_format,
            extra,
            ..Default::default()
        };
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        Ok(ir)
    }

    fn read_response_events(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();
        if data.as_str() == Some(crate::codec::dialect::SSE_DONE_SENTINEL) || !data.is_object() {
            return out;
        }

        let event_type_val = data.get(keys::TYPE).and_then(|t| t.as_str()).unwrap_or("");
        match event_type_val {
            ET_MESSAGE_START => {
                if !state.started {
                    state.started = true;

                    let id = data
                        .get(keys::ID)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    out.push(IrStreamEvent::MessageStart {
                        role: crate::codec::ir::IrRole::Assistant,
                        usage: None,
                        id,
                        created: None,

                        model: data
                            .get(keys::MODEL)
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                    });
                }
            }

            ET_CONTENT_START
                if data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::MESSAGE))
                    .and_then(|m| m.get(keys::CONTENT))
                    .and_then(|c| c.get(keys::TYPE))
                    .and_then(|t| t.as_str())
                    == Some(keys::THINKING) =>
            {
                if cohere_open_thinking_index(state).is_none() {
                    cohere_close_text_slot(state, &mut out);
                    if let Some(index) =
                        cohere_open_thinking_block(state, &mut out, &SOURCE_STREAM_THINKING_START)
                    {
                        if let Some(text) = data
                            .get(keys::DELTA)
                            .and_then(|d| d.get(keys::MESSAGE))
                            .and_then(|m| m.get(keys::CONTENT))
                            .and_then(cohere_thinking_part)
                            .filter(|s| !s.is_empty())
                        {
                            out.push(IrStreamEvent::BlockDelta {
                                index,
                                delta: crate::codec::ir::IrDelta::ThinkingDelta(text.to_string()),
                            });
                        }
                    } else {
                        out.push(cohere_frame_limit_error());
                    }
                }
            }

            ET_CONTENT_DELTA
                if let Some(text) = data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::MESSAGE))
                    .and_then(|m| m.get(keys::CONTENT))
                    .and_then(cohere_thinking_part) =>
            {
                let index = match cohere_open_thinking_index(state) {
                    Some(index) => Some(index),
                    None => {
                        cohere_close_text_slot(state, &mut out);
                        cohere_open_thinking_block(state, &mut out, &SOURCE_STREAM_THINKING_DELTA)
                    }
                };
                let Some(index) = index else {
                    out.push(cohere_frame_limit_error());
                    return out;
                };
                if !text.is_empty() {
                    out.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: crate::codec::ir::IrDelta::ThinkingDelta(text.to_string()),
                    });
                }
            }
            ET_CONTENT_START => {
                if state.text_block_closed {
                    let seen = state
                        .tool_ir_index
                        .range(TEXT_FRAME_BASE..THINKING_FRAME_BASE)
                        .count();
                    let Some(index) = cohere_assign_tool_ir_index(state, TEXT_FRAME_BASE + seen)
                    else {
                        out.push(cohere_frame_limit_error());
                        return out;
                    };
                    state.text_index = Some(index);
                    state.text_block_closed = false;
                }
                cohere_close_thinking_block(state, &mut out);

                if !state.text_block_open {
                    state.text_block_open = true;
                    let ti = cohere_text_ir_index(state);
                    state.block_origins.insert(
                        ti,
                        crate::codec::ir::IrBlockOrigin::at(&SOURCE_STREAM_TEXT_START),
                    );
                    out.push(IrStreamEvent::BlockStart {
                        index: ti,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
            }
            ET_CONTENT_DELTA if !state.text_block_closed => {
                cohere_close_thinking_block(state, &mut out);

                let text_idx = cohere_text_ir_index(state);
                if !state.text_block_open {
                    state.text_block_open = true;
                    state.block_origins.insert(
                        text_idx,
                        crate::codec::ir::IrBlockOrigin::at(&SOURCE_STREAM_TEXT_DELTA),
                    );
                    out.push(IrStreamEvent::BlockStart {
                        index: text_idx,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }

                if let Some(delta_obj) = data.get(keys::DELTA) {
                    if let Some(content_obj) = delta_obj
                        .get(keys::MESSAGE)
                        .and_then(|m| m.get(keys::CONTENT))
                    {
                        if let Some(text) = content_obj.as_str() {
                            if !text.is_empty() {
                                out.push(IrStreamEvent::BlockDelta {
                                    index: text_idx,
                                    delta: crate::codec::ir::IrDelta::TextDelta(text.to_string()),
                                });
                            }
                        } else if let Some(block_obj) = content_obj.as_object() {
                            let ty = block_obj.get(keys::TYPE).and_then(|t| t.as_str());
                            if ty.is_none() || ty == Some(keys::TEXT) {
                                if let Some(text) =
                                    block_obj.get(keys::TEXT).and_then(|t| t.as_str())
                                {
                                    if !text.is_empty() {
                                        out.push(IrStreamEvent::BlockDelta {
                                            index: text_idx,
                                            delta: crate::codec::ir::IrDelta::TextDelta(
                                                text.to_string(),
                                            ),
                                        });
                                    }
                                }
                            }
                        } else if let Some(content_arr) = content_obj.as_array() {
                            for block_val in content_arr {
                                if let Some(block_obj) = block_val.as_object() {
                                    if block_obj.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(keys::TEXT)
                                    {
                                        if let Some(text) =
                                            block_obj.get(keys::TEXT).and_then(|t| t.as_str())
                                        {
                                            out.push(IrStreamEvent::BlockDelta {
                                                index: text_idx,
                                                delta: crate::codec::ir::IrDelta::TextDelta(
                                                    text.to_string(),
                                                ),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some(lp) = data.get(keys::LOGPROBS).and_then(read_cohere_logprob) {
                    out.push(IrStreamEvent::BlockDelta {
                        index: text_idx,
                        delta: crate::codec::ir::IrDelta::LogprobsDelta(vec![lp]),
                    });
                }
            }

            ET_TOOL_PLAN_DELTA if !state.text_block_closed => {
                cohere_close_thinking_block(state, &mut out);
                let plan_idx = cohere_text_ir_index(state);
                if !state.text_block_open {
                    state.text_block_open = true;
                    state.thinking_block_open = true;
                    state.block_origins.insert(
                        plan_idx,
                        crate::codec::ir::IrBlockOrigin::at(&SOURCE_STREAM_TOOL_PLAN),
                    );
                    out.push(IrStreamEvent::BlockStart {
                        index: plan_idx,
                        block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                        refusal: false,
                    });
                }
                if let Some(text) = data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::MESSAGE))
                    .and_then(|m| m.get(TOOL_PLAN))
                    .and_then(|p| p.as_str())
                    .filter(|s| !s.is_empty())
                {
                    out.push(IrStreamEvent::BlockDelta {
                        index: plan_idx,
                        delta: crate::codec::ir::IrDelta::ThinkingDelta(text.to_string()),
                    });
                }
            }
            ET_CONTENT_END => {
                if cohere_open_thinking_index(state).is_some() {
                    cohere_close_thinking_block(state, &mut out);
                } else {
                    cohere_close_text_slot(state, &mut out);
                }
            }
            ET_MESSAGE_END => {
                cohere_close_thinking_block(state, &mut out);
                cohere_close_text_slot(state, &mut out);
                let raw_finish_reason = data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::FINISH_REASON))
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let stop_reason = if raw_finish_reason.is_empty() {
                    None
                } else {
                    Some(read_cohere_stop_reason(raw_finish_reason))
                };

                if stop_reason == Some(crate::codec::ir::IrStopReason::Error) {
                    out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                        class: busbar_contract::upstream::StatusClass::ServerError,
                        provider_signal: Some(raw_finish_reason.to_string()),
                        retry_after: None,
                    }));
                }

                let usage = data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::USAGE))
                    .map(|u| read_cohere_usage(Some(u)))
                    .transpose();
                let usage = match usage {
                    Ok(usage) => usage.unwrap_or(crate::codec::ir::IrUsage::default()),
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };

                out.push(IrStreamEvent::MessageDelta {
                    stop_reason,

                    stop_sequence: None,
                    usage,
                    stop_detail: None,
                });
                out.push(IrStreamEvent::MessageStop);
            }

            ET_TOOL_CALL_START => {
                let frame_idx = match cohere_frame_index(data) {
                    Ok(index) => index,
                    Err(error) => return vec![IrStreamEvent::Error(error)],
                };
                cohere_close_thinking_block(state, &mut out);
                cohere_close_text_slot(state, &mut out);
                let tc = data
                    .get(keys::DELTA)
                    .and_then(|d| d.get(keys::MESSAGE))
                    .and_then(|m| m.get(keys::TOOL_CALLS));
                let id = tc
                    .and_then(|t| t.get(keys::ID))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = tc
                    .and_then(|t| t.get(keys::FUNCTION))
                    .and_then(|f| f.get(keys::NAME))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if let Some(ir_idx) = cohere_assign_tool_ir_index(state, frame_idx) {
                    state.block_origins.insert(
                        ir_idx,
                        crate::codec::ir::IrBlockOrigin::at(&SOURCE_STREAM_TOOL_CALL),
                    );
                    out.push(IrStreamEvent::BlockStart {
                        index: ir_idx,
                        block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
                        refusal: false,
                    });

                    if let Some(args) = tc
                        .and_then(|t| t.get(keys::FUNCTION))
                        .and_then(|f| f.get(keys::ARGUMENTS))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        out.push(IrStreamEvent::BlockDelta {
                            index: ir_idx,
                            delta: crate::codec::ir::IrDelta::InputJsonDelta(args.to_string()),
                        });
                    }
                } else if !state.tool_ir_index.contains_key(&frame_idx) {
                    out.push(cohere_frame_limit_error());
                }
            }
            ET_TOOL_CALL_DELTA => {
                let frame_idx = match cohere_frame_index(data) {
                    Ok(index) => index,
                    Err(error) => return vec![IrStreamEvent::Error(error)],
                };

                if let Some(ir_idx) = state.tool_ir_index.get(&frame_idx).copied() {
                    if let Some(args) = data
                        .get(keys::DELTA)
                        .and_then(|d| d.get(keys::MESSAGE))
                        .and_then(|m| m.get(keys::TOOL_CALLS))
                        .and_then(|t| t.get(keys::FUNCTION))
                        .and_then(|f| f.get(keys::ARGUMENTS))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        out.push(IrStreamEvent::BlockDelta {
                            index: ir_idx,
                            delta: crate::codec::ir::IrDelta::InputJsonDelta(args.to_string()),
                        });
                    }
                }
            }
            ET_TOOL_CALL_END => {
                let frame_idx = match cohere_frame_index(data) {
                    Ok(index) => index,
                    Err(error) => return vec![IrStreamEvent::Error(error)],
                };

                if let Some(ir_idx) = state.tool_ir_index.get(&frame_idx).copied() {
                    out.push(IrStreamEvent::BlockStop { index: ir_idx });
                }
            }

            ET_CITATION_START => {
                if state.text_block_open && !state.thinking_block_open {
                    if let Some(raw) = data
                        .get(keys::DELTA)
                        .and_then(|d| d.get(keys::MESSAGE))
                        .and_then(|m| m.get(keys::CITATIONS))
                    {
                        let cits = if raw.is_array() {
                            super::read_cohere_citations(raw)
                        } else {
                            super::read_cohere_citations(&serde_json::Value::Array(vec![
                                raw.clone()
                            ]))
                        };
                        if !cits.is_empty() {
                            out.push(IrStreamEvent::BlockDelta {
                                index: state.text_index.unwrap_or(0),
                                delta: crate::codec::ir::IrDelta::CitationsDelta(cits),
                            });
                        }
                    }
                }
            }

            ET_CITATION_END => {}

            _ => {}
        }
        out
    }

    /// Only `message-end` bills or fails a stream; an escaped `type` is read whole.
    fn same_proto_reads(&self, _event_type: &str, payload: &[u8]) -> bool {
        crate::codec::json_splice::any_member(payload, |m, v| {
            m.is(keys::TYPE)
                && (v.strip_prefix(b"\"").and_then(|w| w.strip_suffix(b"\""))
                    == Some(ET_MESSAGE_END.as_bytes())
                    || v.contains(&b'\\'))
        })
    }

    fn clone_box(&self) -> Box<dyn ProtocolReader> {
        Box::new(self.clone())
    }

    fn grammar(&self) -> &'static crate::codec::drops::Grammar {
        &GRAMMAR
    }

    fn read_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;
        let mut citation_budget = super::CohereCitationBudget::default();
        let message_val = obj
            .get(keys::MESSAGE)
            .and_then(serde_json::Value::as_object)
            .ok_or_else(ir_parse_error)?;

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();

        if let Some(plan) = message_val.get(TOOL_PLAN).and_then(|p| p.as_str()) {
            if !plan.is_empty() {
                {
                    block_origins
                        .response
                        .push(crate::codec::ir::IrBlockOrigin::at(
                            &SOURCE_RESPONSE_TOOL_PLAN,
                        ));
                    content.push(crate::codec::ir::IrBlock::Thinking {
                        text: plan.to_string(),
                        signature: None,
                        redacted: false,
                        cache_control: None,
                        kind: None,
                        signature_origin: None,
                    })
                };
            }
        }

        let response_citations = message_val
            .get(keys::CITATIONS)
            .map(super::read_cohere_citations)
            .unwrap_or_default();

        if let Some(content_arr) = message_val.get(keys::CONTENT).and_then(|c| c.as_array()) {
            for block_val in content_arr {
                if let Some(block_obj) = block_val.as_object() {
                    match block_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
                        Some(keys::TEXT) => {
                            if let Some(text) = block_obj.get(keys::TEXT).and_then(|t| t.as_str()) {
                                {
                                    block_origins.response.push(
                                        crate::codec::ir::IrBlockOrigin::at(&SOURCE_RESPONSE_TEXT),
                                    );
                                    content.push(crate::codec::ir::IrBlock::Text {
                                        text: text.to_string(),
                                        cache_control: None,
                                        citations: Vec::new(),
                                        refusal: false,
                                    })
                                };
                            }
                        }

                        Some(keys::THINKING) => {
                            if let Some(text) = block_obj
                                .get(keys::THINKING)
                                .and_then(|t| t.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                {
                                    block_origins.response.push(
                                        crate::codec::ir::IrBlockOrigin::at(
                                            &SOURCE_RESPONSE_THINKING,
                                        ),
                                    );
                                    content.push(crate::codec::ir::IrBlock::Thinking {
                                        text: text.to_string(),
                                        signature: None,
                                        redacted: false,
                                        cache_control: None,
                                        kind: None,
                                        signature_origin: None,
                                    })
                                };
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if super::attach_cohere_citations(&mut content, response_citations, &mut citation_budget)? {
            block_origins
                .response
                .push(crate::codec::ir::IrBlockOrigin::at(
                    &SOURCE_RESPONSE_CITATIONS,
                ));
        }

        if let Some(tool_calls_arr) = message_val.get(keys::TOOL_CALLS).and_then(|t| t.as_array()) {
            for tc_val in tool_calls_arr {
                if let Some(func_obj) = tc_val.get(keys::FUNCTION) {
                    let id = tc_val
                        .get(keys::ID)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = func_obj
                        .get(keys::NAME)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = func_obj
                        .get(keys::ARGUMENTS)
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let input = crate::codec::json::parse_str(arguments)
                        .unwrap_or(serde_json::Value::String(arguments.to_string()));
                    {
                        block_origins
                            .response
                            .push(crate::codec::ir::IrBlockOrigin::at(
                                &SOURCE_RESPONSE_TOOL_CALL,
                            ));
                        content.push(crate::codec::ir::IrBlock::ToolUse {
                            id,
                            name,
                            input,
                            cache_control: None,
                            thought_signature: None,
                        })
                    };
                }
            }
        }

        let raw_finish_reason = obj
            .get(keys::FINISH_REASON)
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let stop_reason = if raw_finish_reason.is_empty() {
            None
        } else {
            Some(read_cohere_stop_reason(raw_finish_reason))
        };

        let usage_val = obj.get(keys::USAGE);

        let usage = read_cohere_usage(usage_val)?;

        let logprobs: Vec<crate::codec::ir::IrTokenLogprob> = obj
            .get(keys::LOGPROBS)
            .and_then(|l| l.as_array())
            .map(|items| items.iter().filter_map(read_cohere_logprob).collect())
            .unwrap_or_default();

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        let id = obj
            .get(keys::ID)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        Ok(crate::codec::ir::IrResponse {
            block_origins,
            logprobs,
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason,
            usage,
            model,
            id,
            created: None,
            system_fingerprint: None,
            stop_sequence: None,

            request_echo: None,
            stop_detail: None,
            ..Default::default()
        })
    }
}

/// Cohere's usage counts, as data. The raw `tokens` bucket gives the totals; `billed_units` is the
/// provider-metered bucket (token counts, `search_units`, `classifications`).
const USAGE: &[UsageCount] = &[
    (
        CountSlot::Input,
        CountRead::Zero(&[keys::TOKENS, keys::INPUT_TOKENS]),
    ),
    (CountSlot::Input, CountRead::Less(&[keys::CACHED_TOKENS])),
    (CountSlot::CacheRead, CountRead::Opt(&[keys::CACHED_TOKENS])),
    (
        CountSlot::Output,
        CountRead::Zero(&[keys::TOKENS, keys::OUTPUT_TOKENS]),
    ),
    (
        CountSlot::SearchUnits,
        CountRead::Opt(&[keys::BILLED_UNITS, keys::SEARCH_UNITS]),
    ),
    (
        CountSlot::ProviderInput,
        CountRead::Opt(&[keys::BILLED_UNITS, keys::INPUT_TOKENS]),
    ),
    (
        CountSlot::ProviderOutput,
        CountRead::Opt(&[keys::BILLED_UNITS, keys::OUTPUT_TOKENS]),
    ),
    (
        CountSlot::Classifications,
        CountRead::Opt(&[keys::BILLED_UNITS, CLASSIFICATIONS]),
    ),
];

/// A Cohere `usage` / `meta` object (`None` when absent) → the IR usage, through [`USAGE`]. Every
/// count it reads is ledgered under its class (`IrUsage::to_token_usage`), the billed
/// `classifications` included.
fn read_cohere_usage(
    usage: Option<&serde_json::Value>,
) -> Result<crate::codec::ir::IrUsage, IrError> {
    crate::codec::usage_count::read_usage(VENDOR_NAME, usage, USAGE)
}

#[cfg(test)]
#[path = "tests/unreadable_count_refusal_tests.rs"]
mod unreadable_count_refusal_tests;

#[cfg(test)]
#[path = "tests/native_stream_origin_tests.rs"]
mod native_stream_origin_tests;

fn cohere_frame_limit_error() -> IrStreamEvent {
    IrStreamEvent::Error(busbar_contract::protocol::IrError {
        class: busbar_contract::upstream::StatusClass::ServerError,
        provider_signal: Some("Cohere stream block limit exceeded".to_string()),
        retry_after: None,
    })
}
