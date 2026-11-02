use crate::codec::ir::{IrBlockOrigin, IrBlockOrigins, IrBlockSource};

static MESSAGE_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[]",
};
static REQUEST_CONTENT_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content",
};
static REQUEST_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[]",
};
static REQUEST_TOOL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].tool_calls[]",
};
static REQUEST_FUNCTION_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].function_call",
};
static REQUEST_REFUSAL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].refusal",
};
static RESPONSE_REASONING_CONTENT_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.reasoning_content",
};
static RESPONSE_REASONING_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.reasoning",
};
static RESPONSE_CONTENT_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.content",
};
static RESPONSE_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.content[]",
};
static RESPONSE_REFUSAL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.refusal",
};
static RESPONSE_TOOL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.tool_calls[]",
};
static RESPONSE_FUNCTION_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].message.function_call",
};
static STREAM_REASONING_CONTENT_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.reasoning_content",
};
static STREAM_REASONING_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.reasoning",
};
static STREAM_CONTENT_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.content",
};
static STREAM_REFUSAL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.refusal",
};
static STREAM_TOOL_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.tool_calls[]",
};
static STREAM_FUNCTION_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.function_call",
};
static STREAM_ANNOTATIONS_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].delta.annotations[]",
};
static STREAM_LOGPROBS_SOURCE: IrBlockSource = IrBlockSource {
    dialect: VENDOR_NAME,
    path: "choices[].logprobs.content[]",
};

use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::ir_encode::{synth_response_tool_call_id, tool_input_from_arguments};

/// What a translate attempt cannot carry from this dialect (`codec::drops`).
static GRAMMAR: crate::codec::drops::Grammar = crate::codec::drops::Grammar {
    request_map: super::map::REQUEST,
    parked: super::PARKED,
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

impl ProtocolReader for OpenAiReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;

        Some(read_openai_usage(Some(&v), None).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        let json = crate::codec::json::parse::<serde_json::Value>(body).ok();
        let error_obj = json
            .as_ref()
            .and_then(|j| j.get(keys::ERROR_WORD))
            .and_then(|e| e.as_object());
        let provider_code = error_obj
            .and_then(|e_obj| e_obj.get(keys::CODE))
            .and_then(|c| c.as_str())
            .map(String::from);
        let structured_type = error_obj
            .and_then(|e_obj| e_obj.get(keys::TYPE))
            .and_then(|t| t.as_str())
            .map(String::from);

        let provider_code = provider_code.or_else(|| {
            let oversized_status =
                status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE;
            if !oversized_status {
                return None;
            }
            let message = error_obj
                .and_then(|e_obj| e_obj.get(keys::MESSAGE))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_lowercase();
            if context_length_prose_scan(&message) {
                Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
            } else {
                None
            }
        });

        busbar_contract::upstream::RawUpstreamError {
            http_status: status.as_u16(),
            provider_code,
            structured_type,
            retry_after_secs: None,
        }
    }

    #[cfg(test)]
    fn classify(&self, status: StatusCode, body: &[u8]) -> CanonicalSignal {
        crate::codec::dialect_classify::bearer(status.as_u16(), body)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = IrBlockOrigins::default();

        let mut system_turns_folded: usize = 0;

        let mut system_roles_seen = (false, false);

        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .or_else(|| obj.get(MAX_COMPLETION_TOKENS))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0);

        let max_completion_tokens_was_source =
            !obj.contains_key(keys::MAX_TOKENS) && obj.contains_key(MAX_COMPLETION_TOKENS);

        let response_format = obj
            .get(keys::RESPONSE_FORMAT)
            .and_then(read_openai_response_format);
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();

        let mut message_names = serde_json::Map::new();

        let mut message_extras = serde_json::Map::new();

        let mut legacy_calls: Vec<(String, String, bool)> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            let msgs_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;

            for msg_val in msgs_arr.iter() {
                let role_str = msg_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let content_val = msg_val.get(keys::CONTENT);

                if let Some(cv) = content_val {
                    if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                        return Err(ir_parse_error());
                    }
                }

                let role = match role_str {
                    keys::DEVELOPER | keys::SYSTEM => crate::codec::ir::IrRole::System,
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,

                    keys::TOOL | keys::FUNCTION => crate::codec::ir::IrRole::Tool,
                    _ => return Err(ir_parse_error()),
                };

                if role == crate::codec::ir::IrRole::System {
                    system_turns_folded += 1;
                    if role_str == keys::DEVELOPER {
                        system_roles_seen.1 = true;
                    } else {
                        system_roles_seen.0 = true;
                    }
                    let blocks_before = system_blocks.len();
                    if let Some(content) = content_val {
                        if let Some(text) = content.as_str() {
                            system_blocks.push(crate::codec::ir::IrBlock::text(text.to_string()));
                            block_origins
                                .system
                                .push(IrBlockOrigin::at(&REQUEST_CONTENT_SOURCE));
                        } else if let Some(arr) = content.as_array() {
                            for block_val in arr {
                                if let Some(block) = read_openai_part(block_val)? {
                                    system_blocks.push(block);
                                    block_origins
                                        .system
                                        .push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                                }
                            }
                        }
                    }

                    let had_parts = content_val
                        .and_then(|c| c.as_array())
                        .is_some_and(|a| !a.is_empty());
                    if system_blocks.len() == blocks_before && !had_parts {
                        system_blocks.push(crate::codec::ir::IrBlock::text(String::new()));
                        block_origins
                            .system
                            .push(IrBlockOrigin::at(&MESSAGE_SOURCE));
                    }
                    crate::codec::ir::IrSystemFold::record(
                        &mut system_folds,
                        messages.len(),
                        if role_str == "developer" {
                            crate::codec::ir::IrSystemRole::Developer
                        } else {
                            crate::codec::ir::IrSystemRole::System
                        },
                        blocks_before,
                        system_blocks.len(),
                    );
                } else {
                    let mut msg_content = Vec::new();
                    let mut msg_origins = Vec::new();

                    if role != crate::codec::ir::IrRole::Tool {
                        if let Some(cv) = content_val {
                            if let Some(text) = cv.as_str() {
                                msg_content.push(crate::codec::ir::IrBlock::text(text.to_string()));
                                msg_origins.push(IrBlockOrigin::at(&REQUEST_CONTENT_SOURCE));
                            } else if let Some(arr) = cv.as_array() {
                                for block_val in arr {
                                    if let Some(block) = read_openai_part(block_val)? {
                                        msg_content.push(block);
                                        msg_origins.push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                                    }
                                }
                            }
                        }
                    }

                    if role == crate::codec::ir::IrRole::Assistant {
                        if let Some(tool_calls) = msg_val.get(keys::TOOL_CALLS) {
                            if let Some(tc_arr) = tool_calls.as_array() {
                                for tc_val in tc_arr {
                                    if tc_val.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(keys::CUSTOM)
                                    {
                                        crate::codec::drops::writer_drop!(
                                            crate::codec::drops::wire("messages[].tool_calls[]"),
                                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                            [],
                                            "dropping a replayed custom-tool call on translate: the \
                                             IR carries function tool calls only (no-equivalent)"
                                        );
                                        continue;
                                    }

                                    let id = tc_val
                                        .get(keys::ID)
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                        .ok_or_else(ir_parse_error)?
                                        .to_string();
                                    let Some(func) = tc_val.get(keys::FUNCTION) else {
                                        if unmodeled_tool_call(tc_val) {
                                            continue;
                                        }
                                        return Err(ir_parse_error());
                                    };
                                    let name = func
                                        .get(keys::NAME)
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let input =
                                        tool_input_from_arguments(func.get(keys::ARGUMENTS));

                                    msg_content.push(crate::codec::ir::IrBlock::ToolUse {
                                        id,
                                        name,
                                        input,
                                        cache_control: None,
                                        thought_signature: None,
                                    });
                                    msg_origins.push(IrBlockOrigin::at(&REQUEST_TOOL_SOURCE));
                                }
                            }
                        }
                    }

                    if role == crate::codec::ir::IrRole::Assistant
                        && msg_val.get(keys::TOOL_CALLS).is_none()
                    {
                        if let Some(fc) = msg_val.get(keys::FUNCTION_CALL).filter(|f| f.is_object())
                        {
                            let name = fc
                                .get(keys::NAME)
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let id = synth_response_tool_call_id(legacy_calls.len(), &name);
                            legacy_calls.push((name.clone(), id.clone(), false));
                            msg_content.push(crate::codec::ir::IrBlock::ToolUse {
                                id,
                                name,
                                input: tool_input_from_arguments(fc.get(keys::ARGUMENTS)),
                                cache_control: None,
                                thought_signature: None,
                            });
                            msg_origins.push(IrBlockOrigin::at(&REQUEST_FUNCTION_SOURCE));
                        }
                    }

                    if role == crate::codec::ir::IrRole::Tool {
                        let tool_call_id = if role_str == keys::FUNCTION {
                            let fname = msg_val
                                .get(keys::NAME)
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            match legacy_calls
                                .iter_mut()
                                .rev()
                                .find(|(n, _, matched)| n == fname && !matched)
                            {
                                Some((_, id, matched)) => {
                                    *matched = true;
                                    id.clone()
                                }
                                None => synth_response_tool_call_id(legacy_calls.len(), fname),
                            }
                        } else {
                            msg_val
                                .get(keys::TOOL_CALL_ID)
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string()
                        };

                        let tool_content_origin =
                            if content_val.is_some_and(|v| v.is_string() || v.is_array()) {
                                IrBlockOrigin::at(&REQUEST_CONTENT_SOURCE)
                            } else {
                                IrBlockOrigin::default()
                            };
                        let content_text = match content_val {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(serde_json::Value::Array(parts)) => {
                                let mut acc = String::new();
                                for part in parts {
                                    if let Ok(Some(crate::codec::ir::IrBlock::Text {
                                        text, ..
                                    })) = read_openai_part(part)
                                    {
                                        acc.push_str(&text);
                                    }
                                }
                                acc
                            }
                            Some(_) | None => String::new(),
                        };

                        msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                            tool_use_id: tool_call_id,
                            content: vec![crate::codec::ir::IrBlock::text(content_text)],
                            is_error: false,
                            cache_control: None,
                        });
                        msg_origins.push(IrBlockOrigin {
                            source: Some(&MESSAGE_SOURCE),
                            children: vec![tool_content_origin],
                        });
                    }

                    if let Some(refusal) = msg_val
                        .get(keys::REFUSAL)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        msg_content.push(crate::codec::ir::IrBlock::Text {
                            text: refusal.to_string(),
                            cache_control: None,
                            citations: Vec::new(),
                            refusal: true,
                        });
                        msg_origins.push(IrBlockOrigin::at(&REQUEST_REFUSAL_SOURCE));
                    }

                    if let Some(name) = msg_val
                        .get(keys::NAME)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty() && role_str != keys::FUNCTION)
                    {
                        message_names.insert(messages.len().to_string(), serde_json::json!(name));
                    }

                    if let Some(mo) = msg_val.as_object() {
                        let mut this_extras = serde_json::Map::new();
                        for (k, v) in mo {
                            if !matches!(
                                k.as_str(),
                                keys::ROLE
                                    | keys::CONTENT
                                    | keys::NAME
                                    | keys::TOOL_CALLS
                                    | keys::TOOL_CALL_ID
                                    | keys::REFUSAL
                            ) {
                                this_extras.insert(k.clone(), v.clone());
                            }
                        }

                        if role_str == keys::FUNCTION {
                            this_extras.insert(
                                LEGACY_FUNCTION_ROLE_KEY.to_string(),
                                msg_val
                                    .get(keys::NAME)
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            );
                        }
                        if !this_extras.is_empty() {
                            message_extras.insert(
                                messages.len().to_string(),
                                serde_json::Value::Object(this_extras),
                            );
                        }
                    }
                    messages.push(crate::codec::ir::IrMessage {
                        role,
                        content: msg_content,
                    });
                    block_origins.messages.push(msg_origins);
                }
            }
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        let mut custom_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_val) = obj.get(keys::TOOLS) {
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                if let Some(custom) = super::slots::read_custom_tool(tool_val) {
                    custom_tools.push(custom);
                    continue;
                }
                tools.push(read_openai_tool(tool_val)?);
            }
        } else if let Some(functions) = obj.get(FUNCTIONS).and_then(|f| f.as_array()) {
            for f in functions {
                tools.push(read_openai_tool(&serde_json::json!({
                    (keys::TYPE): TOOL_TYPE_FUNCTION,
                    (keys::FUNCTION): f,
                }))?);
            }
        }

        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        if max_completion_tokens_was_source && max_tokens.is_some() {
            extra.insert(
                MAX_COMPLETION_TOKENS_SENTINEL.to_string(),
                serde_json::Value::Bool(true),
            );
        }

        if !message_names.is_empty() {
            extra.insert(
                MESSAGE_NAMES_SENTINEL.to_string(),
                serde_json::Value::Object(message_names),
            );
        }

        if !message_extras.is_empty() {
            extra.insert(
                MESSAGE_EXTRAS_SENTINEL.to_string(),
                serde_json::Value::Object(message_extras),
            );
        }

        let tool_choice = read_openai_tool_choice(obj.get(keys::TOOL_CHOICE)).or_else(|| match obj
            .get(
            keys::FUNCTION_CALL,
        )? {
            serde_json::Value::String(s) if s == keys::AUTO => {
                Some(crate::codec::ir::IrToolChoice::Auto)
            }
            serde_json::Value::String(s) if s == keys::NONE_WORD => {
                Some(crate::codec::ir::IrToolChoice::None)
            }
            serde_json::Value::Object(o) => {
                o.get(keys::NAME).and_then(|n| n.as_str()).map(|name| {
                    crate::codec::ir::IrToolChoice::Tool {
                        name: name.to_string(),
                    }
                })
            }
            _ => None,
        });

        let user = obj
            .get(keys::USER)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parallel_tool_calls = obj.get(keys::PARALLEL_TOOL_CALLS).and_then(|v| v.as_bool());

        let reasoning_effort_raw = obj.get(REASONING_EFFORT).and_then(|v| v.as_str());

        let reasoning = reasoning_effort_raw.and_then(|raw| match raw {
            keys::NONE_WORD => Some(crate::codec::ir::IrReasoningAsk::Off),
            "xhigh" => Some(crate::codec::ir::IrReasoningAsk::Effort(
                crate::codec::ir::IrReasoningEffort::XHigh,
            )),
            other => crate::codec::ir::IrReasoningEffort::parse(other)
                .map(crate::codec::ir::IrReasoningAsk::Effort),
        });

        if let Some(raw) = reasoning_effort_raw {
            if crate::codec::ir::IrReasoningEffort::parse(raw).is_none() {
                if reasoning.is_none() {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [reasoning_effort = raw,],
                        "reasoning_effort value has no IR word; preserving it verbatim in extra so \
                         a same-protocol OpenAI egress still carries it (a cross-protocol hop \
                         carries no ask)"
                    );
                }
                extra.insert(
                    REASONING_EFFORT.to_string(),
                    serde_json::Value::String(raw.to_string()),
                );
            }
        }

        let logprobs = obj.get(keys::LOGPROBS).and_then(|v| v.as_bool());
        let top_logprobs = obj
            .get(keys::TOP_LOGPROBS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let mut ir = crate::codec::ir::IrRequest {
            block_origins,
            reasoning,
            logprobs,
            top_logprobs,
            user,
            parallel_tool_calls,
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
            hosted_tools: custom_tools,

            system_role: match system_roles_seen {
                (true, false) => Some(crate::codec::ir::IrSystemRole::System),
                (false, true) => Some(crate::codec::ir::IrSystemRole::Developer),
                _ => None,
            },
            ..Default::default()
        };

        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        super::slots::read_tool_choice_slots(obj, &mut ir);
        Ok(ir)
    }

    /// OpenAI's flat stream → IR block-structured events. One chat.completion.chunk
    fn read_response_events(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();

        if data.as_str() == Some(crate::codec::dialect::SSE_DONE_SENTINEL) {
            return out;
        }

        if let Some(error_obj) = data.get(keys::ERROR_WORD).and_then(|e| e.as_object()) {
            let error_type = error_obj.get(keys::TYPE).and_then(|t| t.as_str());
            let code = error_obj.get(keys::CODE).and_then(|c| c.as_str());

            let provider_signal = code
                .filter(|c| !c.is_empty())
                .or(error_type)
                .or_else(|| error_obj.get(keys::MESSAGE).and_then(|m| m.as_str()))
                .map(String::from);
            out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                class: stream_inline_error_class(error_type, code),
                provider_signal,
                retry_after: None,
            }));
            return out;
        }

        if !state.started {
            state.started = true;
            out.push(IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id: data
                    .get(keys::ID)
                    .and_then(|v| v.as_str())
                    .map(String::from),
                created: data.get(CREATED).and_then(|v| v.as_u64()),
                model: data
                    .get(keys::MODEL)
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }

        let choices_arr = data.get(CHOICES).and_then(|c| c.as_array());

        if let Some(arr) = choices_arr {
            if arr.len() > 1 && !state.multi_candidate_warned {
                state.multi_candidate_warned = true;
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire("choices[]"),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [choices = arr.len(),],
                    "openai stream chunk carried multiple choices; only choices[0] survives IR translation — a cross-protocol hop drops the rest (a same-protocol relay preserves all)"
                );
            }
        }
        let choice0 = choices_arr.and_then(|a| a.first());
        let delta = choice0.and_then(|c| c.get(keys::DELTA));

        if let Some((reasoning, reasoning_source)) = delta
            .and_then(|d| {
                d.get(REASONING_CONTENT)
                    .map(|v| (v, &STREAM_REASONING_CONTENT_SOURCE))
                    .or_else(|| {
                        d.get(keys::REASONING)
                            .map(|v| (v, &STREAM_REASONING_SOURCE))
                    })
            })
            .and_then(|(r, source)| r.as_str().map(|r| (r, source)))
            .filter(|(r, _)| !r.is_empty())
        {
            if !state.thinking_block_open {
                let answer_started = state.text_block_open
                    || state.text_block_closed
                    || state.text_index.is_some()
                    || !state.open_tools.is_empty()
                    || state.next_ir_index > 0;
                let index = if answer_started {
                    close_text_block(state, &mut out);
                    let i = state.claim_ir_index();
                    state.tool_ir_index.insert(LATE_THINKING_KEY, i);
                    i
                } else {
                    state.reasoning_seen = true;
                    state.tool_ir_index.remove(&LATE_THINKING_KEY);
                    0
                };
                state.thinking_block_open = true;
                state
                    .block_origins
                    .insert(index, IrBlockOrigin::at(reasoning_source));
                out.push(IrStreamEvent::BlockStart {
                    index,
                    block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                    refusal: false,
                });
            }
            out.push(IrStreamEvent::BlockDelta {
                index: thinking_index(state),
                delta: crate::codec::ir::IrDelta::ThinkingDelta(reasoning.to_string()),
            });
        }

        let refusal_delta = delta
            .and_then(|d| d.get(keys::REFUSAL))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty());
        if refusal_delta.is_some() {
            state.refusal_seen = true;
        }
        let content_delta = delta
            .and_then(|d| d.get(keys::CONTENT))
            .and_then(|c| c.as_str());

        let (text_delta, is_refusal) = match (content_delta, refusal_delta) {
            (Some(c), _) if !c.is_empty() => (Some(c), false),
            (_, Some(r)) => (Some(r), true),
            (c, None) => (c, false),
        };

        if state.text_block_open
            && text_delta.is_some_and(|t| !t.is_empty())
            && is_refusal != open_text_is_refusal(state)
        {
            close_text_block(state, &mut out);
        }
        if let Some(content) = text_delta.filter(|c| !(state.text_block_closed && c.is_empty())) {
            close_thinking_block(state, &mut out);

            if state.text_block_closed {
                state.text_block_closed = false;
                state.text_index = None;
            }
            let ti = match state.text_index {
                Some(i) => i,
                None => {
                    let i = state.claim_ir_index();
                    state.text_index = Some(i);
                    i
                }
            };
            if !state.text_block_open {
                state.text_block_open = true;
                if is_refusal {
                    state.tool_ir_index.insert(REFUSAL_TEXT_KEY, ti);
                }
                state.block_origins.insert(
                    ti,
                    IrBlockOrigin::at(if is_refusal {
                        &STREAM_REFUSAL_SOURCE
                    } else {
                        &STREAM_CONTENT_SOURCE
                    }),
                );
                out.push(IrStreamEvent::BlockStart {
                    index: ti,
                    block: crate::codec::ir::IrBlockMeta::Text,
                    refusal: is_refusal,
                });
            }
            out.push(IrStreamEvent::BlockDelta {
                index: ti,
                delta: crate::codec::ir::IrDelta::TextDelta(content.to_string()),
            });
        }

        let lp_entries = if state.text_block_closed {
            Vec::new()
        } else {
            crate::codec::logprob_wire::read_token_logprobs(
                choice0.and_then(|c| c.get(keys::LOGPROBS)),
            )
        };
        if !lp_entries.is_empty() {
            if !state.text_block_open {
                close_thinking_block(state, &mut out);
                state.text_block_open = true;
                let ti = state.text_index.unwrap_or_else(|| {
                    let i = state.claim_ir_index();
                    state.text_index = Some(i);
                    i
                });
                state
                    .block_origins
                    .insert(ti, IrBlockOrigin::at(&STREAM_LOGPROBS_SOURCE));
                out.push(IrStreamEvent::BlockStart {
                    index: ti,
                    block: crate::codec::ir::IrBlockMeta::Text,
                    refusal: false,
                });
            }
            out.push(IrStreamEvent::BlockDelta {
                index: state
                    .text_index
                    .expect("text_index set above when block opens"),
                delta: crate::codec::ir::IrDelta::LogprobsDelta(lp_entries),
            });
        }

        let citations = delta
            .and_then(|d| d.get(keys::ANNOTATIONS))
            .map(super::super::url_citation_wire::read_url_annotations)
            .unwrap_or_default();
        if !citations.is_empty() {
            if state.text_block_closed && !state.text_block_open {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire("choices[].delta.annotations[]"),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [citations = citations.len(),],
                    "dropping streamed url_citation annotations that arrived after their text \
                     block closed; they are NOT forwarded on this cross-protocol stream"
                );
            } else {
                if !state.text_block_open {
                    close_thinking_block(state, &mut out);
                    state.text_block_open = true;
                    let ti = state.text_index.unwrap_or_else(|| {
                        let i = state.claim_ir_index();
                        state.text_index = Some(i);
                        i
                    });
                    state
                        .block_origins
                        .insert(ti, IrBlockOrigin::at(&STREAM_ANNOTATIONS_SOURCE));
                    out.push(IrStreamEvent::BlockStart {
                        index: ti,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
                if let Some(ti) = state.text_index {
                    out.push(IrStreamEvent::BlockDelta {
                        index: ti,
                        delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                    });
                }
            }
        }

        let tool_items: Vec<(
            usize,
            Option<&str>,
            Option<&serde_json::Value>,
            &'static IrBlockSource,
        )> = match delta
            .and_then(|d| d.get(keys::TOOL_CALLS))
            .and_then(|t| t.as_array())
        {
            Some(tcs) => tcs
                .iter()
                .map(|tc| {
                    let oai_idx = tc
                        .get(keys::INDEX)
                        .and_then(|i| i.as_u64())
                        .map_or(0, |v| v.min(MAX_TOOL_INDEX) as usize);
                    (
                        oai_idx,
                        tc.get(keys::ID).and_then(|i| i.as_str()),
                        tc.get(keys::FUNCTION),
                        &STREAM_TOOL_SOURCE,
                    )
                })
                .collect(),
            None => delta
                .and_then(|d| d.get(keys::FUNCTION_CALL))
                .filter(|f| f.is_object())
                .map(|f| {
                    vec![(
                        LEGACY_FUNCTION_CALL_KEY,
                        None,
                        Some(f),
                        &STREAM_FUNCTION_SOURCE,
                    )]
                })
                .unwrap_or_default(),
        };
        if !tool_items.is_empty() {
            close_thinking_block(state, &mut out);

            close_text_block(state, &mut out);
            for (oai_idx, tc_id, func, tool_source) in tool_items {
                if let Some(name) = func
                    .and_then(|f| f.get(keys::NAME))
                    .and_then(|n| n.as_str())
                {
                    let already_open = state.open_tools.contains(&oai_idx);
                    if !already_open && state.open_tools.len() < MAX_OPEN_TOOLS {
                        let id = match tc_id {
                            Some(id) => id.to_string(),
                            None if oai_idx == LEGACY_FUNCTION_CALL_KEY => {
                                synth_response_tool_call_id(0, name)
                            }
                            None => String::new(),
                        };
                        let ir_idx = state.claim_ir_index();
                        state.open_tools.insert(oai_idx);

                        state.tool_ir_index.insert(oai_idx, ir_idx);
                        state
                            .block_origins
                            .insert(ir_idx, IrBlockOrigin::at(tool_source));
                        out.push(IrStreamEvent::BlockStart {
                            index: ir_idx,
                            block: crate::codec::ir::IrBlockMeta::ToolUse {
                                id,
                                name: name.to_string(),
                            },
                            refusal: false,
                        });
                    }
                }
                if let Some(args) = func
                    .and_then(|f| f.get(keys::ARGUMENTS))
                    .and_then(|a| a.as_str())
                {
                    let Some(&index) = state.tool_ir_index.get(&oai_idx) else {
                        continue;
                    };
                    out.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: crate::codec::ir::IrDelta::InputJsonDelta(args.to_string()),
                    });
                }
            }
        }

        let chunk_usage = data
            .get(keys::USAGE)
            .filter(|u| u.is_object())
            .map(|u| read_openai_usage(Some(u), data.get(keys::SERVICE_TIER)));
        let chunk_usage = match chunk_usage.transpose() {
            Ok(usage) => usage,
            Err(refusal) => {
                out.push(IrStreamEvent::Error(refusal));
                return out;
            }
        };

        let finish_reason = choice0
            .and_then(|c| c.get(keys::FINISH_REASON))
            .and_then(|r| r.as_str());
        if let Some(fr) = finish_reason {
            close_thinking_block(state, &mut out);
            if state.text_block_open {
                state.text_block_open = false;

                if let Some(ti) = state.text_index {
                    out.push(IrStreamEvent::BlockStop { index: ti });
                }
            }

            let tool_ir_index = std::mem::take(&mut state.tool_ir_index);
            for oai_idx in std::mem::take(&mut state.open_tools) {
                if let Some(&index) = tool_ir_index.get(&oai_idx) {
                    out.push(IrStreamEvent::BlockStop { index });
                }
            }
            let stop_reason = match read_openai_stop_reason(fr) {
                crate::codec::ir::IrStopReason::EndTurn if state.refusal_seen => {
                    Some(crate::codec::ir::IrStopReason::Refusal)
                }
                other => Some(other),
            };
            let usage = chunk_usage.unwrap_or(IrUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,

                detail: crate::codec::ir::IrUsageDetail {
                    service_tier: crate::codec::carry::read_word(
                        super::map::WORDS_SERVED_TIER,
                        data.get(keys::SERVICE_TIER),
                    ),
                    ..Default::default()
                },
            });
            out.push(IrStreamEvent::MessageDelta {
                stop_reason,

                stop_sequence: None,
                usage,
                stop_detail: None,
            });
            out.push(IrStreamEvent::MessageStop);
        } else if let Some(usage) = chunk_usage {
            if choice0.is_none() {
                out.push(IrStreamEvent::MessageDelta {
                    stop_reason: None,
                    stop_sequence: None,
                    usage,
                    stop_detail: None,
                });
            }
        }

        out
    }

    /// A chunk bills or fails only through a top-level `error` OBJECT, a `usage` OBJECT, or a
    fn same_proto_reads(&self, _event_type: &str, payload: &[u8]) -> bool {
        use crate::codec::json_splice::{any_member, first_element};
        any_member(payload, |m, v| {
            ((m.is(keys::ERROR_WORD) || m.is(keys::USAGE)) && v.starts_with(b"{"))
                || (m.is(CHOICES)
                    && first_element(v).is_some_and(|choice| {
                        any_member(choice, |m, v| {
                            m.is(keys::FINISH_REASON) && v.starts_with(b"\"")
                        })
                    }))
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

        let choices_val = obj.get(CHOICES).ok_or_else(ir_parse_error)?;
        let choices = choices_val.as_array().ok_or_else(ir_parse_error)?;

        if choices.is_empty() {
            return Err(ir_parse_error());
        }

        if choices.len() > 1 {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("choices[]"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [choices = choices.len(),],
                "openai response carried multiple choices; only choices[0] survives IR translation — a cross-protocol hop drops the rest (a same-protocol relay preserves all)"
            );
        }

        let choice = &choices[0];

        let message_val = choice.get(keys::MESSAGE).ok_or_else(ir_parse_error)?;

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = IrBlockOrigins::default();

        for (key, source) in [
            (REASONING_CONTENT, &RESPONSE_REASONING_CONTENT_SOURCE),
            (keys::REASONING, &RESPONSE_REASONING_SOURCE),
        ] {
            if let Some(r) = message_val.get(key).and_then(|v| v.as_str()) {
                if !r.is_empty() {
                    content.push(crate::codec::ir::IrBlock::Thinking {
                        text: r.to_string(),
                        signature: None,
                        redacted: false,
                        cache_control: None,
                        kind: None,
                        signature_origin: None,
                    });
                    block_origins.response.push(IrBlockOrigin::at(source));
                    break;
                }
            }
        }

        if let Some(content_val) = message_val.get(keys::CONTENT) {
            if let Some(text) = content_val.as_str() {
                if !text.is_empty() {
                    let citations = message_val
                        .get(keys::ANNOTATIONS)
                        .map(super::super::url_citation_wire::read_url_annotations)
                        .unwrap_or_default();
                    content.push(crate::codec::ir::IrBlock::Text {
                        text: text.to_string(),
                        cache_control: None,
                        citations,
                        refusal: false,
                    });
                    block_origins
                        .response
                        .push(IrBlockOrigin::at(&RESPONSE_CONTENT_SOURCE));
                }
            } else if let Some(arr) = content_val.as_array() {
                for block_val in arr {
                    let Some(block) = read_openai_part(block_val)? else {
                        continue;
                    };

                    if matches!(block, crate::codec::ir::IrBlock::Image { .. }) {
                        let _native_drop_origin = crate::codec::drops::block_scope(Some(
                            &IrBlockOrigin::at(&RESPONSE_PART_SOURCE),
                        ));
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::IMAGE,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping an image content part from an OpenAI Chat response message: the \
                             completion response shape carries no image output; the block is NOT \
                             emitted"
                        );
                    } else {
                        content.push(block);
                        block_origins
                            .response
                            .push(IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                    }
                }
            }
        }

        let mut saw_refusal = false;
        if let Some(text) = message_val.get(keys::REFUSAL).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                saw_refusal = true;

                content.push(crate::codec::ir::IrBlock::Text {
                    text: text.to_string(),
                    cache_control: None,
                    citations: Vec::new(),
                    refusal: true,
                });
                block_origins
                    .response
                    .push(IrBlockOrigin::at(&RESPONSE_REFUSAL_SOURCE));
            }
        }

        if let Some(tool_calls_val) = message_val.get(keys::TOOL_CALLS) {
            if let Some(tc_arr) = tool_calls_val.as_array() {
                for (tc_ordinal, tc_val) in tc_arr.iter().enumerate() {
                    let raw_id = tc_val.get(keys::ID).and_then(|v| v.as_str()).unwrap_or("");
                    let Some(func) = tc_val.get(keys::FUNCTION) else {
                        if unmodeled_tool_call(tc_val) {
                            continue;
                        }
                        return Err(ir_parse_error());
                    };
                    let name = func
                        .get(keys::NAME)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let id = if raw_id.is_empty() {
                        synth_response_tool_call_id(tc_ordinal, &name)
                    } else {
                        raw_id.to_string()
                    };
                    let input = tool_input_from_arguments(func.get(keys::ARGUMENTS));

                    content.push(crate::codec::ir::IrBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control: None,
                        thought_signature: None,
                    });
                    block_origins
                        .response
                        .push(IrBlockOrigin::at(&RESPONSE_TOOL_SOURCE));
                }
            }
        }

        if !content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. }))
        {
            if let Some(fc) = message_val
                .get(keys::FUNCTION_CALL)
                .filter(|f| f.is_object())
            {
                let name = fc
                    .get(keys::NAME)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                content.push(crate::codec::ir::IrBlock::ToolUse {
                    id: synth_response_tool_call_id(0, &name),
                    input: tool_input_from_arguments(fc.get(keys::ARGUMENTS)),
                    name,
                    cache_control: None,
                    thought_signature: None,
                });
                block_origins
                    .response
                    .push(IrBlockOrigin::at(&RESPONSE_FUNCTION_SOURCE));
            }
        }

        let finish_reason = choice
            .get(keys::FINISH_REASON)
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let mut stop_reason = if finish_reason.is_empty() {
            None
        } else {
            Some(read_openai_stop_reason(finish_reason))
        };

        if saw_refusal && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn) {
            stop_reason = Some(crate::codec::ir::IrStopReason::Refusal);
        }

        let usage_val = obj.get(keys::USAGE);

        let usage = read_openai_usage(usage_val, obj.get(keys::SERVICE_TIER))?;

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        let id = obj.get(keys::ID).and_then(|v| v.as_str()).map(String::from);
        let created = obj.get(CREATED).and_then(|v| v.as_u64());
        let system_fingerprint = obj
            .get(SYSTEM_FINGERPRINT)
            .and_then(|v| v.as_str())
            .map(String::from);

        let logprobs =
            crate::codec::logprob_wire::read_token_logprobs(choices[0].get(keys::LOGPROBS));

        Ok(crate::codec::ir::IrResponse {
            block_origins,
            logprobs,
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason,
            usage,
            model,
            id,
            created,
            system_fingerprint,
            stop_sequence: None,

            request_echo: None,
            stop_detail: None,
            safety: super::read_moderation(obj.get(MODERATION)),
            audio: super::read_message_audio(
                choice.get(keys::MESSAGE).and_then(|m| m.get(super::AUDIO)),
            ),
        })
    }
}

/// The top-level moderation results of a Chat completion.
const MODERATION: &str = "moderation";

/// The `tool_ir_index` key that records the IR index of a Thinking block opened AFTER the answer
const LATE_THINKING_KEY: usize = usize::MAX;

/// The `open_tools` / `tool_ir_index` join key of a streamed legacy `delta.function_call` (OAI-08).
const LEGACY_FUNCTION_CALL_KEY: usize = usize::MAX - 1;

/// The `tool_ir_index` key that records the IR index of the text block opened for `delta.refusal`
const REFUSAL_TEXT_KEY: usize = usize::MAX - 2;

/// Is the open text block the refusal block?
fn open_text_is_refusal(state: &crate::codec::ir::StreamDecodeState) -> bool {
    state.text_block_open
        && state.text_index.is_some()
        && state.tool_ir_index.get(&REFUSAL_TEXT_KEY).copied() == state.text_index
}

/// The IR index of the open (or last) Thinking block: the recorded late index, else the reserved 0.
fn thinking_index(state: &crate::codec::ir::StreamDecodeState) -> usize {
    state
        .tool_ir_index
        .get(&LATE_THINKING_KEY)
        .copied()
        .unwrap_or(0)
}

/// Close the Thinking block if it is open, at the index it was opened with.
fn close_thinking_block(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if state.thinking_block_open {
        state.thinking_block_open = false;
        out.push(IrStreamEvent::BlockStop {
            index: thinking_index(state),
        });
    }
}

/// Close the text block if it is open and latch it closed, so later text opens a fresh block.
fn close_text_block(state: &mut crate::codec::ir::StreamDecodeState, out: &mut Vec<IrStreamEvent>) {
    if state.text_block_open {
        state.text_block_open = false;
        state.text_block_closed = true;
        if let Some(ti) = state.text_index {
            out.push(IrStreamEvent::BlockStop { index: ti });
        }
    }
}

#[cfg(test)]
#[path = "tests/unreadable_count_refusal_tests.rs"]
mod unreadable_count_refusal_tests;

/// A tool call of a type the IR does not model (a `custom` call, or a type OpenAI adds later): its
fn unmodeled_tool_call(tc_val: &serde_json::Value) -> bool {
    tc_val
        .get(keys::TYPE)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|t| t != keys::FUNCTION)
}

#[cfg(test)]
mod native_origin_tests {
    use super::*;
    use crate::codec::drops;
    use crate::codec::ir::IrBlockOrigin;
    use serde_json::json;

    fn projected_paths(origin: Option<&IrBlockOrigin>) -> Vec<String> {
        let (_, paths) = drops::scope(
            drops::Seam {
                direction: drops::Direction::Request,
                ingress: VENDOR_NAME,
                egress: crate::codec::anthropic::DECL.name,
            },
            || {
                drops::with_block_origin(origin, || {
                    drops::note(drops::Dropped::member(
                        drops::IMAGE,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        "projection probe",
                    ));
                })
            },
        );
        paths
    }

    #[test]
    fn folded_system_sources_and_aggregate_tool_children_keep_native_containers() {
        let body = json!({"messages": [
            {"role": "system", "content": "rules"},
            {"role": "developer", "content": [{"type": "futurePart"}, {"type": "text", "text": "rules"}]},
            {"role": "user", "content": [{"type": "futurePart"}, {"type": "text", "text": "question"}]},
            {"role": "tool", "tool_call_id": "call_lookup", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            {"role": "tool", "tool_call_id": "call_missing"}
        ]});
        let request = OpenAiReader.read_request(&body).unwrap();
        let [scalar, array] = request.block_origins.system.as_slice() else {
            panic!("two folded native entries");
        };
        assert_eq!(projected_paths(Some(scalar)), ["messages[].content"]);
        assert_eq!(projected_paths(Some(array)), ["messages[].content[]"]);
        let [user, tool, missing] = request.block_origins.messages.as_slice() else {
            panic!("system entries fold out of messages");
        };
        let [user_part] = user.as_slice() else {
            panic!("unsupported part has no origin");
        };
        assert_eq!(projected_paths(Some(user_part)), ["messages[].content[]"]);
        let [tool] = tool.as_slice() else {
            panic!("tool result");
        };
        let [child] = tool.children.as_slice() else {
            panic!("array folds to one text child");
        };
        assert_eq!(projected_paths(Some(child)), ["messages[].content"]);
        let [missing] = missing.as_slice() else {
            panic!("missing tool result");
        };
        let [synthetic] = missing.children.as_slice() else {
            panic!("synthetic empty text");
        };
        assert!(synthetic.source.is_none());
    }

    #[test]
    fn response_origin_does_not_shift_after_a_skipped_image_part() {
        let body = json!({"choices": [{"message": {
            "reasoning": "consider", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.test/image"}},
                {"type": "text", "text": "answer"}
            ], "refusal": "cannot comply"
        }}]});
        let response = OpenAiReader
            .read_response(crate::codec::test_host::HOST, &body)
            .unwrap();
        let [reasoning, content, refusal] = response.block_origins.response.as_slice() else {
            panic!("image output is omitted");
        };
        assert_eq!(
            projected_paths(Some(reasoning)),
            ["choices[].message.reasoning"]
        );
        assert_eq!(
            projected_paths(Some(content)),
            ["choices[].message.content[]"]
        );
        assert_eq!(
            projected_paths(Some(refusal)),
            ["choices[].message.refusal"]
        );
        assert_eq!(
            response.block_origins.response.len(),
            response.content.len()
        );
    }

    #[test]
    fn stream_sources_distinguish_legacy_tools_refusals_and_annotations() {
        let cases = [
            (
                json!({"choices": [{"delta": {"function_call": {"name": "lookup", "arguments": "{}"}}}]}),
                "choices[].delta.function_call",
            ),
            (
                json!({"choices": [{"delta": {"tool_calls": [{"index": 17, "id": "call_lookup", "function": {"name": "lookup", "arguments": "{}"}}]}}]}),
                "choices[].delta.tool_calls[]",
            ),
            (
                json!({"choices": [{"delta": {"refusal": "cannot comply"}}]}),
                "choices[].delta.refusal",
            ),
            (
                json!({"choices": [{"delta": {"reasoning": "consider"}}]}),
                "choices[].delta.reasoning",
            ),
            (
                json!({"choices": [{"delta": {"annotations": [{"type": "url_citation", "url_citation": {"url": "https://example.test/source", "title": "source"}}]}}]}),
                "choices[].delta.annotations[]",
            ),
        ];
        for (body, expected) in cases {
            let mut state = crate::codec::ir::StreamDecodeState::default();
            let events = OpenAiReader.read_response_events(
                crate::codec::test_host::HOST,
                "",
                &body,
                &mut state,
            );
            let mut saw_start = false;
            for event in events {
                if let IrStreamEvent::BlockStart { index, .. } = event {
                    saw_start = true;
                    assert_eq!(projected_paths(state.block_origins.get(&index)), [expected]);
                }
            }
            assert!(saw_start);
        }
        let mut state = crate::codec::ir::StreamDecodeState::default();
        OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &json!({"choices": [{"delta": {"tool_calls": [{"index": 17, "function": {"arguments": "{}"}}]}}]}), &mut state);
        assert!(
            state.block_origins.is_empty(),
            "arguments without a successful BlockStart create no origin"
        );
    }
}
