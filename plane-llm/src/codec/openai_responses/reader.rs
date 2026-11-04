use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::ir_encode::{synth_response_tool_call_id, tool_input_from_arguments};

/// Preserve the legacy item index for the first part and reserve a disjoint, bounded
/// range for subsequent parts. Never clamp distinct malformed wire identities together.
fn part_index(
    data: &serde_json::Value,
    state: &mut crate::codec::ir::StreamDecodeState,
) -> Option<usize> {
    let output = wire_index(data, OUTPUT_INDEX)?;
    let content = wire_index(data, "content_index")?;
    if let Some(&index) = state.responses_parts.get(&(output, content)) {
        return Some(index);
    }
    if state.responses_parts.len() >= MAX_OPEN_TOOLS {
        return None;
    }
    let index = if content == 0 {
        output
    } else {
        MAX_OUTPUT_INDEX + 1 + state.responses_parts.len()
    };
    state.responses_parts.insert((output, content), index);
    Some(index)
}

fn wire_index(data: &serde_json::Value, key: &str) -> Option<usize> {
    let value = match data.get(key) {
        None => 0,
        Some(value) => value.as_u64()?,
    };
    (value <= MAX_OUTPUT_INDEX as u64).then_some(value as usize)
}

fn invalid_stream_index() -> IrStreamEvent {
    IrStreamEvent::Error(IrError {
        class: StatusClass::ServerError,
        provider_signal: Some("invalid_response_index".to_string()),
        retry_after: None,
    })
}

fn emit_refusal_text(
    index: usize,
    text: &str,
    snapshot: bool,
    source: &'static crate::codec::ir::IrBlockSource,
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if text.is_empty() || state.responses_closed.contains(&index) {
        return;
    }
    state.refusal_seen = true;
    let seen = state.tool_ir_index.contains_key(&index);
    let text_key = index + TEXT_INDEX_KEY_OFFSET;
    if (snapshot && seen) || (seen && !state.open_tools.contains(&text_key)) {
        return;
    }
    if !seen {
        if state.open_tools.contains(&index) || state.open_tools.contains(&text_key) {
            return;
        }
        if state.open_tools.len() >= MAX_OPEN_TOOLS {
            out.push(invalid_stream_index());
            return;
        }
        state.open_tools.insert(text_key);
        state.tool_ir_index.insert(index, index);
        state
            .block_origins
            .insert(index, crate::codec::ir::IrBlockOrigin::at(source));
        out.push(IrStreamEvent::BlockStart {
            index,
            block: crate::codec::ir::IrBlockMeta::Text,
            refusal: true,
        });
    }
    out.push(IrStreamEvent::BlockDelta {
        index,
        delta: crate::codec::ir::IrDelta::TextDelta(text.to_string()),
    });
}

fn close_open_blocks(
    out: &mut Vec<IrStreamEvent>,
    state: &mut crate::codec::ir::StreamDecodeState,
) {
    let mut indices: Vec<usize> = state
        .open_tools
        .iter()
        .map(|&key| {
            if key >= TEXT_INDEX_KEY_OFFSET {
                key - TEXT_INDEX_KEY_OFFSET
            } else {
                key
            }
        })
        .collect();
    state.open_tools.clear();
    indices.sort_unstable();
    indices.dedup();
    for index in indices {
        state.responses_closed.insert(index);
        out.push(IrStreamEvent::BlockStop { index });
    }
}

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
        defaults: super::ANSWER_DEFAULTS,
        reroot: None,
    }),
    stream_carried: Some(crate::codec::drops::Carried {
        map: super::map::STREAM_PATHS,
        code: super::STREAM_CODE,
        drops: super::STREAM_DROPS,
        defaults: super::ANSWER_DEFAULTS,
        reroot: Some(super::STREAM_REROOT),
    }),
    stream_keyed_by_event: false,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for ResponsesReader {
    fn recover_response_usage(
        &self,
        body: &serde_json::Value,
    ) -> Option<busbar_contract::billing::TokenUsage> {
        let usage = body.get(keys::USAGE).filter(|usage| usage.is_object())?;

        Some(
            read_responses_usage(Some(usage), Some(body))
                .ok()?
                .to_token_usage(),
        )
    }

    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;

        Some(read_responses_usage(Some(&v), None).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        let (provider_code, structured_type) =
            match crate::codec::json::parse::<serde_json::Value>(body) {
                Ok(json) => {
                    let error = json.get(keys::ERROR_WORD).and_then(|e| e.as_object());
                    let provider_code = error
                        .and_then(|e_obj| e_obj.get(keys::CODE))
                        .and_then(|c| c.as_str())
                        .map(String::from);
                    let structured_type = error
                        .and_then(|e_obj| e_obj.get(keys::TYPE))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                    (provider_code, structured_type)
                }
                Err(_) => (None, None),
            };

        let provider_code = provider_code.or_else(|| {
            let oversized_status =
                status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE;
            if !oversized_status {
                return None;
            }
            let lower = String::from_utf8_lossy(body).to_lowercase();
            if crate::codec::dialect::context_length_prose_scan(&lower) {
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

        if obj.is_empty() {
            return Err(ir_parse_error());
        }

        if obj
            .get(PREVIOUS_RESPONSE_ID)
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(ir_parse_error());
        }
        if obj
            .get(FIELD_STORE)
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(ir_parse_error());
        }

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();

        let mut system_turns_folded: usize = 0;

        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        let (mut saw_system, mut saw_developer, mut saw_instructions) = (false, false, false);

        if let Some(instructions) = obj.get(keys::INSTRUCTIONS).and_then(|v| v.as_str()) {
            if !instructions.is_empty() {
                saw_instructions = true;
                system_blocks.push(crate::codec::ir::IrBlock::text(instructions.to_string()));
                block_origins
                    .system
                    .push(crate::codec::ir::IrBlockOrigin::at(&origin::INSTRUCTIONS));
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();

        if let Some(input_val) = obj.get(keys::INPUT) {
            if !input_val.is_null() && !input_val.is_string() && !input_val.is_array() {
                return Err(ir_parse_error());
            }
            if input_val.is_string() {
                let text = input_val.as_str().unwrap_or("").to_string();
                messages.push(crate::codec::ir::IrMessage {
                    role: crate::codec::ir::IrRole::User,
                    content: vec![crate::codec::ir::IrBlock::Text {
                        text,
                        cache_control: None,
                        citations: Vec::new(),
                        refusal: false,
                    }],
                });
                block_origins
                    .messages
                    .push(vec![crate::codec::ir::IrBlockOrigin::at(
                        &origin::INPUT_STRING,
                    )]);
            } else if let Some(arr) = input_val.as_array() {
                for item in arr {
                    match item.get(keys::TYPE).and_then(|t| t.as_str()) {
                        Some(CONTENT_TYPE_INPUT_TEXT) => {
                            let text = item
                                .get(keys::TEXT)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            messages.push(crate::codec::ir::IrMessage {
                                role: crate::codec::ir::IrRole::User,
                                content: vec![crate::codec::ir::IrBlock::Text {
                                    text,
                                    cache_control: None,
                                    citations: Vec::new(),
                                    refusal: false,
                                }],
                            });
                            block_origins
                                .messages
                                .push(vec![crate::codec::ir::IrBlockOrigin::at(
                                    &origin::INPUT_TEXT,
                                )]);
                        }
                        Some(INPUT_IMAGE) => {
                            if let Some(block) = responses_input_image_block(item) {
                                messages.push(crate::codec::ir::IrMessage {
                                    role: crate::codec::ir::IrRole::User,
                                    content: vec![block],
                                });
                                block_origins.messages.push(vec![
                                    crate::codec::ir::IrBlockOrigin::at(&origin::INPUT_IMAGE_ITEM),
                                ]);
                            }
                        }
                        Some(CONTENT_TYPE_OUTPUT_TEXT) => {
                            let text = item
                                .get(keys::TEXT)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();

                            let citations = item
                                .get(keys::ANNOTATIONS)
                                .map(super::super::url_citation_wire::read_url_annotations)
                                .unwrap_or_default();
                            messages.push(crate::codec::ir::IrMessage {
                                role: crate::codec::ir::IrRole::Assistant,
                                content: vec![crate::codec::ir::IrBlock::Text {
                                    text,
                                    cache_control: None,
                                    citations,
                                    refusal: false,
                                }],
                            });
                            block_origins
                                .messages
                                .push(vec![crate::codec::ir::IrBlockOrigin::at(
                                    &origin::OUTPUT_TEXT_INPUT,
                                )]);
                        }
                        Some(ITEM_TYPE_FUNCTION_CALL) => {
                            let call_id = item
                                .get(CALL_ID)
                                .and_then(|c| c.as_str())
                                .filter(|s| !s.is_empty())
                                .ok_or_else(ir_parse_error)?
                                .to_string();
                            let name = item
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();

                            let input = tool_input_from_arguments(item.get(keys::ARGUMENTS));

                            messages.push(crate::codec::ir::IrMessage {
                                role: crate::codec::ir::IrRole::Assistant,
                                content: vec![crate::codec::ir::IrBlock::ToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                    cache_control: None,
                                    thought_signature: None,
                                }],
                            });
                            block_origins
                                .messages
                                .push(vec![crate::codec::ir::IrBlockOrigin::at(
                                    &origin::FUNCTION_CALL_INPUT,
                                )]);
                        }
                        Some(FUNCTION_CALL_OUTPUT) => {
                            let call_id = item
                                .get(CALL_ID)
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output_val = item.get(keys::OUTPUT);
                            let (content_blocks, child_origins) = match output_val {
                                Some(serde_json::Value::String(text)) => (
                                    vec![crate::codec::ir::IrBlock::text(text.clone())],
                                    vec![crate::codec::ir::IrBlockOrigin::at(
                                        &origin::TOOL_OUTPUT_STRING,
                                    )],
                                ),
                                Some(serde_json::Value::Array(arr)) => {
                                    responses_parts_with_origins(arr, origin::PartSite::ToolOutput)
                                }
                                _ => (Vec::new(), Vec::new()),
                            };

                            messages.push(crate::codec::ir::IrMessage {
                                role: crate::codec::ir::IrRole::Tool,
                                content: vec![crate::codec::ir::IrBlock::ToolResult {
                                    tool_use_id: call_id,
                                    content: content_blocks,
                                    is_error: false,
                                    cache_control: None,
                                }],
                            });
                            block_origins
                                .messages
                                .push(vec![crate::codec::ir::IrBlockOrigin {
                                    source: Some(&origin::FUNCTION_OUTPUT_INPUT),
                                    children: child_origins,
                                }]);
                        }
                        Some(ITEM_TYPE_MESSAGE) => {
                            if let Some(cv) = item.get(keys::CONTENT) {
                                if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                                    return Err(ir_parse_error());
                                }
                            }
                            let role_str =
                                item.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");

                            if role_str == keys::SYSTEM || role_str == keys::DEVELOPER {
                                if !messages.is_empty() {
                                    tracing::warn!(
                                        role = role_str,
                                        "a Responses `{role_str}` item appeared AFTER an earlier \
                                         conversational turn; busbar hoists it to apply from the \
                                         start of the conversation (the IR has no positioned \
                                         system-turn concept), which changes when this instruction \
                                         takes effect relative to the native Responses behavior"
                                    );
                                }
                                system_turns_folded += 1;
                                saw_system |= role_str == keys::SYSTEM;
                                saw_developer |= role_str == keys::DEVELOPER;
                                let blocks_before = system_blocks.len();
                                push_system_content(
                                    &mut system_blocks,
                                    &mut block_origins.system,
                                    item.get(keys::CONTENT),
                                );
                                crate::codec::ir::IrSystemFold::record(
                                    &mut system_folds,
                                    messages.len(),
                                    if role_str == keys::DEVELOPER {
                                        crate::codec::ir::IrSystemRole::Developer
                                    } else {
                                        crate::codec::ir::IrSystemRole::System
                                    },
                                    blocks_before,
                                    system_blocks.len(),
                                );
                                continue;
                            }
                            let role = match role_str {
                                keys::USER => Some(crate::codec::ir::IrRole::User),
                                keys::ASSISTANT => Some(crate::codec::ir::IrRole::Assistant),
                                _ => None,
                            };
                            if let Some(role) = role {
                                if let Some((msg_content, origins)) =
                                    message_content_blocks(item.get(keys::CONTENT))
                                {
                                    messages.push(crate::codec::ir::IrMessage {
                                        role,
                                        content: msg_content,
                                    });
                                    block_origins.messages.push(origins);
                                }
                            }
                        }
                        Some(ITEM_TYPE_REASONING) => {
                            let text = read_reasoning_text(item);
                            let signature =
                                read_reasoning_encrypted_content(item).map(String::from);

                            if item
                                .get(keys::ID)
                                .and_then(|i| i.as_str())
                                .is_some_and(|i| !i.is_empty())
                            {
                                crate::codec::drops::writer_drop!(
                                    crate::codec::drops::wire("input[].id"),
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [reasoning_id =
                                        item.get(keys::ID).and_then(|i| i.as_str()).unwrap_or(""),],
                                    "dropping reasoning input item `id` on Responses ir parse: the \
                                     IR Thinking block models no reasoning-item id; the reasoning \
                                     text/encrypted_content survive, the specific id is re-minted"
                                );
                            }
                            if !text.is_empty() || signature.is_some() {
                                let (signature, signature_origin) =
                                    crate::codec::ir::sig_envelope::read_carried_opt(
                                        signature,
                                        Some(crate::codec::ir::IrSignatureOrigin::OpenAi),
                                    );
                                messages.push(crate::codec::ir::IrMessage {
                                    role: crate::codec::ir::IrRole::Assistant,
                                    content: vec![crate::codec::ir::IrBlock::Thinking {
                                        text: text.into_owned(),
                                        signature,
                                        redacted: false,
                                        cache_control: None,
                                        kind: super::slots::reasoning_kind(item),
                                        signature_origin,
                                    }],
                                });
                                block_origins.messages.push(vec![
                                    crate::codec::ir::IrBlockOrigin::at(&origin::REASONING_INPUT),
                                ]);
                            }
                        }
                        Some(_) | None => {}
                    }

                    if item.get(keys::TYPE).is_none() && item.get(keys::ROLE).is_some() {
                        let role_str = item.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");
                        let content_val = item.get(keys::CONTENT);

                        if let Some(cv) = content_val {
                            if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                                return Err(ir_parse_error());
                            }
                        }

                        if role_str == keys::SYSTEM || role_str == keys::DEVELOPER {
                            if !messages.is_empty() {
                                tracing::warn!(
                                    role = role_str,
                                    "a Responses `{role_str}` item appeared AFTER an earlier \
                                     conversational turn; busbar hoists it to apply from the \
                                     start of the conversation (the IR has no positioned \
                                     system-turn concept), which changes when this instruction \
                                     takes effect relative to the native Responses behavior"
                                );
                            }
                            system_turns_folded += 1;
                            saw_system |= role_str == keys::SYSTEM;
                            saw_developer |= role_str == keys::DEVELOPER;
                            let blocks_before = system_blocks.len();
                            push_system_content(
                                &mut system_blocks,
                                &mut block_origins.system,
                                content_val,
                            );
                            crate::codec::ir::IrSystemFold::record(
                                &mut system_folds,
                                messages.len(),
                                if role_str == keys::DEVELOPER {
                                    crate::codec::ir::IrSystemRole::Developer
                                } else {
                                    crate::codec::ir::IrSystemRole::System
                                },
                                blocks_before,
                                system_blocks.len(),
                            );
                            continue;
                        }

                        let role = match role_str {
                            keys::USER => crate::codec::ir::IrRole::User,
                            keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                            _ => continue,
                        };

                        if let Some((msg_content, origins)) = message_content_blocks(content_val) {
                            messages.push(crate::codec::ir::IrMessage {
                                role,
                                content: msg_content,
                            });
                            block_origins.messages.push(origins);
                        }
                    }
                }
            }
        } else if !obj.contains_key(keys::INSTRUCTIONS) {
            return Err(ir_parse_error());
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        let mut hosted_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_val) = obj.get(keys::TOOLS) {
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                let type_str = tool_val.get(keys::TYPE).and_then(|t| t.as_str());
                let is_function = match type_str {
                    Some(keys::FUNCTION) => true,
                    Some(_) => false,
                    None => tool_val.get(keys::NAME).is_some(),
                };
                if !is_function {
                    if let Some(hosted) = super::slots::read_hosted_tool(tool_val) {
                        hosted_tools.push(hosted);
                        continue;
                    }
                    tools.push(crate::codec::ir::IrTool {
                        name: String::new(),
                        description: None,
                        input_schema: serde_json::Value::Null,
                        cache_control: None,
                        hosted: Some(tool_val.clone()),
                        strict: None,
                    });
                    continue;
                }

                let name = tool_val
                    .get(keys::NAME)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = tool_val
                    .get(keys::DESCRIPTION)
                    .and_then(|v| v.as_str().map(String::from));
                let input_schema = tool_val
                    .get(keys::PARAMETERS)
                    .or_else(|| tool_val.get("input_schema"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                tools.push(crate::codec::ir::IrTool {
                    name,
                    description,
                    input_schema,
                    cache_control: None,
                    hosted: None,

                    strict: tool_val.get(keys::STRICT).and_then(|v| v.as_bool()),
                });
            }
        }

        let max_tokens = obj
            .get(INCOMPLETE_REASON_MAX_OUTPUT)
            .and_then(|v| v.as_u64())
            .filter(|&v| v > 0)
            .and_then(|v| u32::try_from(v).ok());

        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (tool_choice, allowed_tools) = read_responses_tool_choice(obj.get(keys::TOOL_CHOICE));

        let response_format = read_text_format(obj.get(keys::TEXT));

        for (key, value) in obj.iter() {
            if key == keys::TEXT {
                if let Some(text_obj) = value.as_object() {
                    let remainder: serde_json::Map<String, serde_json::Value> = text_obj
                        .iter()
                        .filter(|(k, _)| k.as_str() != keys::FORMAT)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if !remainder.is_empty() {
                        extra.insert(keys::TEXT.to_string(), serde_json::Value::Object(remainder));
                    }
                }
                continue;
            }
            if crate::codec::carry::models(super::map::REQUEST, key) {
                continue;
            }
            extra.insert(key.clone(), value.clone());
        }

        if let Some(model_val) = obj.get(keys::MODEL) {
            extra.insert(keys::MODEL.to_string(), model_val.clone());
        }

        let reasoning = obj
            .get(keys::REASONING)
            .and_then(|r| r.get(keys::EFFORT))
            .and_then(|v| v.as_str())
            .and_then(read_responses_reasoning_effort);

        let parallel_tool_calls = obj.get(keys::PARALLEL_TOOL_CALLS).and_then(|v| v.as_bool());

        let top_logprobs = obj
            .get(keys::TOP_LOGPROBS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let logprobs = obj
            .get(INCLUDE)
            .and_then(|i| i.as_array())
            .is_some_and(|arr| {
                arr.iter()
                    .any(|v| v.as_str() == Some(INCLUDE_OUTPUT_TEXT_LOGPROBS))
            })
            .then_some(true);

        let user = obj
            .get(keys::USER)
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
            .map(String::from);

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            logprobs,
            top_logprobs,
            user,
            parallel_tool_calls,
            system: system_blocks,
            block_origins,
            system_turns_folded,
            system_folds,
            messages,
            tools,
            max_tokens,
            tool_choice,
            stream,
            response_format,
            allowed_tools,
            hosted_tools,
            system_role: super::slots::system_role(saw_system, saw_developer, saw_instructions),
            extra,
            ..Default::default()
        };
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        Ok(ir)
    }

    fn read_response_events(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();

        if !data.is_object() {
            return out;
        }

        // Validate before payload guards so malformed identities cannot silently
        // disappear on tool, reasoning, empty-delta, or completion paths.
        if matches!(
            event_type,
            EVT_OUTPUT_ITEM_ADDED
                | EVT_OUTPUT_ITEM_DONE
                | EVT_CONTENT_PART_DONE
                | EVT_OUTPUT_TEXT_DELTA
                | EVT_OUTPUT_TEXT_ANNOTATION_ADDED
                | EVT_REFUSAL_DELTA
                | EVT_REFUSAL_DONE
                | EVT_FUNCTION_CALL_ARGS_DELTA
                | EVT_REASONING_TEXT_DELTA
                | EVT_REASONING_SUMMARY_TEXT_DELTA
        ) && (wire_index(data, OUTPUT_INDEX).is_none()
            || wire_index(data, "content_index").is_none())
        {
            return vec![invalid_stream_index()];
        }

        match event_type {
            EVT_RESPONSE_CREATED | "response.in_progress" => {
                if !state.started {
                    state.started = true;

                    let resp = data.get(keys::RESPONSE);
                    let id = resp
                        .and_then(|r| r.get(keys::ID))
                        .and_then(|i| i.as_str())
                        .map(String::from);
                    let created = resp
                        .and_then(|r| r.get(keys::CREATED_AT))
                        .and_then(|c| c.as_u64());
                    let model = resp
                        .and_then(|r| r.get(keys::MODEL))
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    out.push(IrStreamEvent::MessageStart {
                        role: crate::codec::ir::IrRole::Assistant,
                        usage: None,
                        id,
                        created,
                        model,
                    });
                }
            }

            EVT_OUTPUT_ITEM_ADDED => {
                if let Some(item_obj) = data.get(ITEM) {
                    if item_obj.get(keys::TYPE).and_then(|t| t.as_str())
                        == Some(ITEM_TYPE_FUNCTION_CALL)
                    {
                        let raw_call_id =
                            item_obj.get(CALL_ID).and_then(|c| c.as_str()).unwrap_or("");
                        let name = item_obj
                            .get(keys::NAME)
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        if let Some(output_index) = data.get(OUTPUT_INDEX).and_then(|i| i.as_u64())
                        {
                            let call_id = if raw_call_id.is_empty() {
                                synth_response_tool_call_id(
                                    usize::try_from(output_index).unwrap_or(usize::MAX),
                                    &name,
                                )
                            } else {
                                raw_call_id.to_string()
                            };

                            let Some(idx) = wire_index(data, OUTPUT_INDEX) else {
                                out.push(invalid_stream_index());
                                return out;
                            };

                            let already_open = state.open_tools.contains(&idx)
                                || state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET));
                            if !already_open
                                && !state.responses_closed.contains(&idx)
                                && state.open_tools.len() >= MAX_OPEN_TOOLS
                            {
                                out.push(invalid_stream_index());
                                return out;
                            }
                            if !already_open
                                && !state.responses_closed.contains(&idx)
                                && state.open_tools.len() < MAX_OPEN_TOOLS
                            {
                                state.open_tools.insert(idx);
                                state.block_origins.insert(
                                    idx,
                                    crate::codec::ir::IrBlockOrigin::at(&origin::STREAM_FUNCTION),
                                );
                                out.push(IrStreamEvent::BlockStart {
                                    index: idx,
                                    block: crate::codec::ir::IrBlockMeta::ToolUse {
                                        id: call_id,
                                        name,
                                    },
                                    refusal: false,
                                });
                            }
                        }
                    } else if item_obj.get(keys::TYPE).and_then(|t| t.as_str())
                        == Some(ITEM_TYPE_REASONING)
                        && data.get(OUTPUT_INDEX).and_then(|i| i.as_u64()).is_some()
                    {
                        let Some(idx) = wire_index(data, OUTPUT_INDEX) else {
                            out.push(invalid_stream_index());
                            return out;
                        };
                        let already_open = state.open_tools.contains(&idx)
                            || state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET));
                        if !already_open
                            && !state.responses_closed.contains(&idx)
                            && state.open_tools.len() >= MAX_OPEN_TOOLS
                        {
                            out.push(invalid_stream_index());
                            return out;
                        }
                        if !already_open
                            && !state.responses_closed.contains(&idx)
                            && state.open_tools.len() < MAX_OPEN_TOOLS
                        {
                            state.open_tools.insert(idx);
                            state.block_origins.insert(
                                idx,
                                crate::codec::ir::IrBlockOrigin::at(&origin::STREAM_REASONING),
                            );
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,

                                block: crate::codec::ir::IrBlockMeta::Thinking {
                                    kind: data.get(ITEM).and_then(super::slots::reasoning_kind),
                                },
                                refusal: false,
                            });
                        }
                    }
                }
            }

            EVT_REASONING_TEXT_DELTA | EVT_REASONING_SUMMARY_TEXT_DELTA => {
                let delta = data
                    .get(keys::DELTA)
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                if !delta.is_empty() {
                    let Some(idx) = wire_index(data, OUTPUT_INDEX) else {
                        out.push(invalid_stream_index());
                        return out;
                    };
                    if state.responses_closed.contains(&idx) {
                        return out;
                    }

                    if !state.open_tools.contains(&idx)
                        && !state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET))
                    {
                        if state.open_tools.len() >= MAX_OPEN_TOOLS {
                            out.push(invalid_stream_index());
                            return out;
                        }
                        state.open_tools.insert(idx);

                        let kind = if event_type == EVT_REASONING_TEXT_DELTA {
                            crate::codec::ir::IrThinkingKind::Full
                        } else {
                            crate::codec::ir::IrThinkingKind::Summary
                        };
                        let source = if event_type == EVT_REASONING_TEXT_DELTA {
                            &origin::STREAM_REASONING_TEXT
                        } else {
                            &origin::STREAM_REASONING_SUMMARY
                        };
                        state
                            .block_origins
                            .insert(idx, crate::codec::ir::IrBlockOrigin::at(source));
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Thinking { kind: Some(kind) },
                            refusal: false,
                        });
                    }
                    out.push(IrStreamEvent::BlockDelta {
                        index: idx,
                        delta: crate::codec::ir::IrDelta::ThinkingDelta(delta),
                    });
                }
            }

            EVT_OUTPUT_TEXT_DELTA => {
                let delta = data
                    .get(keys::DELTA)
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();

                if !delta.is_empty() {
                    let Some(idx) = part_index(data, state) else {
                        out.push(invalid_stream_index());
                        return out;
                    };
                    if state.responses_closed.contains(&idx) {
                        return out;
                    }

                    let text_key = idx + TEXT_INDEX_KEY_OFFSET;
                    if state.open_tools.contains(&idx) {
                        return out;
                    }
                    let already_open = state.open_tools.contains(&text_key);
                    if !already_open {
                        if state.open_tools.len() >= MAX_OPEN_TOOLS {
                            out.push(invalid_stream_index());
                            return out;
                        }
                        state.open_tools.insert(text_key);
                        state.block_origins.insert(
                            idx,
                            crate::codec::ir::IrBlockOrigin::at(&origin::STREAM_TEXT),
                        );
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Text,
                            refusal: false,
                        });
                    }
                    out.push(IrStreamEvent::BlockDelta {
                        index: idx,
                        delta: crate::codec::ir::IrDelta::TextDelta(delta),
                    });

                    let lps = read_responses_logprobs(data.get(keys::LOGPROBS));
                    if !lps.is_empty() {
                        out.push(IrStreamEvent::BlockDelta {
                            index: idx,
                            delta: crate::codec::ir::IrDelta::LogprobsDelta(lps),
                        });
                    }
                }
            }

            EVT_REFUSAL_DELTA | EVT_REFUSAL_DONE => {
                let Some(idx) = part_index(data, state) else {
                    out.push(invalid_stream_index());
                    return out;
                };
                if state.responses_closed.contains(&idx) {
                    return out;
                }
                let done = event_type == EVT_REFUSAL_DONE;
                let field = if done { keys::REFUSAL } else { keys::DELTA };
                let text = data.get(field).and_then(|v| v.as_str()).unwrap_or("");
                let source = if done {
                    &origin::STREAM_REFUSAL_DONE
                } else {
                    &origin::STREAM_REFUSAL_DELTA
                };
                emit_refusal_text(idx, text, done, source, state, &mut out);
                if done
                    && state.tool_ir_index.contains_key(&idx)
                    && state.open_tools.remove(&(idx + TEXT_INDEX_KEY_OFFSET))
                {
                    state.responses_closed.insert(idx);
                    out.push(IrStreamEvent::BlockStop { index: idx });
                }
            }

            EVT_OUTPUT_TEXT_ANNOTATION_ADDED => {
                let Some(idx) = part_index(data, state) else {
                    out.push(invalid_stream_index());
                    return out;
                };
                if state.responses_closed.contains(&idx) {
                    return out;
                }
                if state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET)) {
                    if let Some(annotation) = data.get("annotation") {
                        let annotations = serde_json::Value::Array(vec![annotation.clone()]);
                        let mut citations =
                            super::super::url_citation_wire::read_url_annotations(&annotations);
                        citations.extend(super::read_file_annotations(Some(&annotations)));
                        if !citations.is_empty() {
                            out.push(IrStreamEvent::BlockDelta {
                                index: idx,
                                delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                            });
                        }
                    }
                }
            }

            EVT_FUNCTION_CALL_ARGS_DELTA => {
                let delta = data
                    .get(keys::DELTA)
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                if !delta.is_empty() && data.get(OUTPUT_INDEX).and_then(|i| i.as_u64()).is_some() {
                    let Some(idx) = wire_index(data, OUTPUT_INDEX) else {
                        out.push(invalid_stream_index());
                        return out;
                    };

                    if state.open_tools.contains(&idx) {
                        out.push(IrStreamEvent::BlockDelta {
                            index: idx,
                            delta: crate::codec::ir::IrDelta::InputJsonDelta(delta),
                        });
                    }
                }
            }

            EVT_OUTPUT_ITEM_DONE | EVT_CONTENT_PART_DONE => {
                let Some(output) = wire_index(data, OUTPUT_INDEX) else {
                    out.push(invalid_stream_index());
                    return out;
                };
                if event_type == EVT_CONTENT_PART_DONE {
                    let Some(idx) = part_index(data, state) else {
                        out.push(invalid_stream_index());
                        return out;
                    };
                    if state.open_tools.remove(&(idx + TEXT_INDEX_KEY_OFFSET)) {
                        state.responses_closed.insert(idx);
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    }
                } else {
                    if state.open_tools.remove(&output) {
                        if let Some(sig) = data
                            .get(ITEM)
                            .filter(|it| {
                                it.get(keys::TYPE).and_then(|t| t.as_str())
                                    == Some(ITEM_TYPE_REASONING)
                            })
                            .and_then(read_reasoning_encrypted_content)
                        {
                            out.push(IrStreamEvent::BlockDelta {
                                index: output,
                                delta: crate::codec::ir::IrDelta::SignatureDelta(sig.to_string()),
                            });
                        }
                        state.responses_closed.insert(output);
                        out.push(IrStreamEvent::BlockStop { index: output });
                    }
                    let indices: Vec<usize> = state
                        .responses_parts
                        .iter()
                        .filter_map(|(&(item, _), &idx)| (item == output).then_some(idx))
                        .collect();
                    for idx in indices {
                        if state.open_tools.remove(&(idx + TEXT_INDEX_KEY_OFFSET)) {
                            state.responses_closed.insert(idx);
                            out.push(IrStreamEvent::BlockStop { index: idx });
                        }
                    }
                }
            }

            EVT_RESPONSE_COMPLETED | EVT_RESPONSE_FAILED | EVT_RESPONSE_INCOMPLETE => {
                if let Some(response_obj) = data.get(keys::RESPONSE) {
                    let status = response_obj
                        .get(keys::STATUS)
                        .and_then(|s| s.as_str())
                        .unwrap_or("");

                    if status == STATUS_FAILED {
                        let provider_signal = response_obj
                            .get(keys::ERROR_WORD)
                            .and_then(|e| e.get(keys::CODE))
                            .and_then(|c| c.as_str())
                            .or_else(|| {
                                response_obj
                                    .get(keys::ERROR_WORD)
                                    .and_then(|e| e.get(keys::TYPE))
                                    .and_then(|t| t.as_str())
                            })
                            .map(String::from)
                            .or_else(|| Some(SIGNAL_RESPONSE_FAILED.to_string()));

                        let class = class_for_response_failed(
                            provider_signal.as_deref().unwrap_or(SIGNAL_RESPONSE_FAILED),
                        );

                        let usage = match response_obj
                            .get(keys::USAGE)
                            .filter(|u| !u.is_null())
                            .map(|u| read_responses_usage(Some(u), Some(response_obj)))
                            .transpose()
                        {
                            Ok(usage) => usage,
                            Err(refusal) => {
                                out.push(IrStreamEvent::Error(refusal));
                                return out;
                            }
                        };
                        out.push(IrStreamEvent::Error(IrError {
                            class,
                            provider_signal,
                            retry_after: None,
                        }));
                        close_open_blocks(&mut out, state);

                        if let Some(usage) = usage {
                            out.push(IrStreamEvent::MessageDelta {
                                stop_reason: Some(crate::codec::ir::IrStopReason::Error),
                                stop_sequence: None,
                                usage,
                                stop_detail: None,
                            });
                        }
                        out.push(IrStreamEvent::MessageStop);
                        return out;
                    }

                    let stop_reason = match status {
                        STATUS_COMPLETED | "" => Some(crate::codec::ir::IrStopReason::EndTurn),

                        STATUS_INCOMPLETE => response_obj
                            .get(INCOMPLETE_DETAILS)
                            .and_then(|d| d.get(keys::REASON))
                            .and_then(|r| r.as_str())
                            .map(read_responses_incomplete_reason),
                        _ => None,
                    };

                    let stop_reason = if stop_reason
                        == Some(crate::codec::ir::IrStopReason::EndTurn)
                        && response_obj
                            .get(keys::OUTPUT)
                            .and_then(|o| o.as_array())
                            .is_some_and(|items| {
                                items.iter().any(|it| {
                                    it.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(ITEM_TYPE_FUNCTION_CALL)
                                })
                            }) {
                        Some(crate::codec::ir::IrStopReason::ToolUse)
                    } else {
                        stop_reason
                    };

                    if let Some(items) = response_obj.get(keys::OUTPUT).and_then(|o| o.as_array()) {
                        for (item_pos, item) in items.iter().enumerate() {
                            let Some(content) = item.get(keys::CONTENT).and_then(|c| c.as_array())
                            else {
                                continue;
                            };
                            for (content_pos, block) in content.iter().enumerate() {
                                if block.get(keys::TYPE).and_then(|t| t.as_str())
                                    != Some(keys::REFUSAL)
                                {
                                    continue;
                                }
                                if let Some(text) =
                                    block.get(keys::REFUSAL).and_then(|r| r.as_str())
                                {
                                    let Some(idx) = part_index(
                                        &serde_json::json!({
                                            "output_index": item_pos, "content_index": content_pos
                                        }),
                                        state,
                                    ) else {
                                        out.push(invalid_stream_index());
                                        return out;
                                    };
                                    emit_refusal_text(
                                        idx,
                                        text,
                                        true,
                                        match event_type {
                                            EVT_RESPONSE_COMPLETED => {
                                                &origin::STREAM_REFUSAL_COMPLETED_SNAPSHOT
                                            }
                                            EVT_RESPONSE_FAILED => {
                                                &origin::STREAM_REFUSAL_FAILED_SNAPSHOT
                                            }
                                            _ => &origin::STREAM_REFUSAL_INCOMPLETE_SNAPSHOT,
                                        },
                                        state,
                                        &mut out,
                                    );
                                }
                            }
                        }
                    }
                    let stop_reason = if state.refusal_seen
                        && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn)
                    {
                        Some(crate::codec::ir::IrStopReason::Refusal)
                    } else {
                        stop_reason
                    };

                    let usage = response_obj
                        .get(keys::USAGE)
                        .map(|u| read_responses_usage(Some(u), Some(response_obj)))
                        .transpose();
                    let usage = match usage {
                        Ok(usage) => usage.unwrap_or(crate::codec::ir::IrUsage {
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_creation_input_tokens: None,
                            cache_read_input_tokens: None,
                            detail: crate::codec::ir::IrUsageDetail {
                                service_tier: crate::codec::carry::read_word(
                                    super::map::WORDS_SERVED_TIER,
                                    response_obj.get(keys::SERVICE_TIER),
                                ),
                                ..Default::default()
                            },
                        }),
                        Err(refusal) => {
                            out.push(IrStreamEvent::Error(refusal));
                            return out;
                        }
                    };

                    close_open_blocks(&mut out, state);
                    out.push(IrStreamEvent::MessageDelta {
                        stop_reason,

                        stop_sequence: None,
                        usage,
                        stop_detail: None,
                    });
                    out.push(IrStreamEvent::MessageStop);
                } else if event_type == EVT_RESPONSE_FAILED {
                    let provider_signal = SIGNAL_RESPONSE_FAILED;
                    out.push(IrStreamEvent::Error(IrError {
                        class: class_for_response_failed(provider_signal),
                        provider_signal: Some(provider_signal.to_string()),
                        retry_after: None,
                    }));
                    close_open_blocks(&mut out, state);
                    out.push(IrStreamEvent::MessageStop);
                } else {
                    let stop_reason =
                        (event_type == EVT_RESPONSE_COMPLETED).then_some(if state.refusal_seen {
                            crate::codec::ir::IrStopReason::Refusal
                        } else {
                            crate::codec::ir::IrStopReason::EndTurn
                        });
                    let usage = crate::codec::ir::IrUsage::default();
                    close_open_blocks(&mut out, state);
                    out.push(IrStreamEvent::MessageDelta {
                        stop_reason,
                        stop_sequence: None,
                        usage,
                        stop_detail: None,
                    });
                    out.push(IrStreamEvent::MessageStop);
                }
            }

            "error" => {
                let signal = data
                    .get(keys::CODE)
                    .and_then(|code| code.as_str())
                    .unwrap_or(SIGNAL_RESPONSE_FAILED);
                out.push(IrStreamEvent::Error(IrError {
                    class: class_for_response_failed(signal),
                    provider_signal: Some(signal.to_string()),
                    retry_after: None,
                }));
                close_open_blocks(&mut out, state);
                out.push(IrStreamEvent::MessageStop);
            }
            _ => {}
        }

        out
    }

    fn read_response(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let status = obj.get(keys::STATUS).and_then(|s| s.as_str()).unwrap_or("");

        if status == STATUS_FAILED {
            let provider_signal = obj
                .get(keys::ERROR_WORD)
                .and_then(|e| e.get(keys::CODE))
                .and_then(|c| c.as_str())
                .or_else(|| {
                    obj.get(keys::ERROR_WORD)
                        .and_then(|e| e.get(keys::TYPE))
                        .and_then(|t| t.as_str())
                })
                .map(String::from)
                .or_else(|| Some(SIGNAL_RESPONSE_FAILED.to_string()));

            let class = class_for_response_failed(
                provider_signal.as_deref().unwrap_or(SIGNAL_RESPONSE_FAILED),
            );
            return Err(IrError {
                class,
                provider_signal,
                retry_after: None,
            });
        }

        let mut stop_reason: Option<crate::codec::ir::IrStopReason> = match status {
            STATUS_COMPLETED => Some(crate::codec::ir::IrStopReason::EndTurn),
            STATUS_INCOMPLETE => obj
                .get(INCOMPLETE_DETAILS)
                .and_then(|d| d.get(keys::REASON))
                .and_then(|r| r.as_str())
                .map(read_responses_incomplete_reason),
            _ => None,
        };

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();

        let mut saw_refusal = false;
        let mut logprobs: Vec<crate::codec::ir::IrTokenLogprob> = Vec::new();
        if let Some(output_arr) = obj.get(keys::OUTPUT).and_then(|o| o.as_array()) {
            for (item_ordinal, item) in output_arr.iter().enumerate() {
                let item_type = item.get(keys::TYPE).and_then(|t| t.as_str()).unwrap_or("");

                match item_type {
                    ITEM_TYPE_MESSAGE => {
                        if let Some(content_arr) =
                            item.get(keys::CONTENT).and_then(|c| c.as_array())
                        {
                            for block_item in content_arr {
                                let block_type = block_item
                                    .get(keys::TYPE)
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");

                                if block_type == CONTENT_TYPE_OUTPUT_TEXT {
                                    if let Some(text) =
                                        block_item.get(keys::TEXT).and_then(|t| t.as_str())
                                    {
                                        let mut citations = block_item
                                            .get(keys::ANNOTATIONS)
                                            .map(super::super::url_citation_wire::read_url_annotations)
                                            .unwrap_or_default();
                                        citations.extend(super::read_file_annotations(
                                            block_item.get(keys::ANNOTATIONS),
                                        ));

                                        logprobs.extend(read_responses_logprobs(
                                            block_item.get(keys::LOGPROBS),
                                        ));
                                        content.push(crate::codec::ir::IrBlock::Text {
                                            text: text.to_string(),
                                            cache_control: None,
                                            citations,
                                            refusal: false,
                                        });
                                        block_origins.response.push(
                                            crate::codec::ir::IrBlockOrigin::at(
                                                &origin::RESPONSE_TEXT,
                                            ),
                                        );
                                    }
                                } else if block_type == keys::REFUSAL {
                                    if let Some(text) =
                                        block_item.get(keys::REFUSAL).and_then(|t| t.as_str())
                                    {
                                        saw_refusal = true;

                                        content.push(crate::codec::ir::IrBlock::Text {
                                            text: text.to_string(),
                                            cache_control: None,
                                            citations: Vec::new(),
                                            refusal: true,
                                        });
                                        block_origins.response.push(
                                            crate::codec::ir::IrBlockOrigin::at(
                                                &origin::RESPONSE_REFUSAL,
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    ITEM_TYPE_FUNCTION_CALL => {
                        let name = item
                            .get(keys::NAME)
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();

                        let raw_call_id = item.get(CALL_ID).and_then(|c| c.as_str()).unwrap_or("");
                        let call_id = if raw_call_id.is_empty() {
                            synth_response_tool_call_id(item_ordinal, &name)
                        } else {
                            raw_call_id.to_string()
                        };

                        let input = tool_input_from_arguments(item.get(keys::ARGUMENTS));

                        content.push(crate::codec::ir::IrBlock::ToolUse {
                            id: call_id,
                            name,
                            input,
                            cache_control: None,
                            thought_signature: None,
                        });
                        block_origins
                            .response
                            .push(crate::codec::ir::IrBlockOrigin::at(
                                &origin::RESPONSE_FUNCTION,
                            ));
                    }

                    ITEM_TYPE_REASONING => {
                        let text = read_reasoning_text(item);
                        let signature = read_reasoning_encrypted_content(item).map(String::from);

                        if !text.is_empty() || signature.is_some() {
                            let (signature, signature_origin) =
                                crate::codec::ir::sig_envelope::read_carried_opt(
                                    signature,
                                    Some(crate::codec::ir::IrSignatureOrigin::OpenAi),
                                );
                            content.push(crate::codec::ir::IrBlock::Thinking {
                                text: text.into_owned(),
                                signature,
                                redacted: false,
                                cache_control: None,
                                kind: super::slots::reasoning_kind(item),
                                signature_origin,
                            });
                            block_origins
                                .response
                                .push(crate::codec::ir::IrBlockOrigin::at(
                                    &origin::RESPONSE_REASONING,
                                ));
                        }
                    }

                    super::ITEM_TYPE_WEB_SEARCH_CALL => {
                        content.push(super::read_web_search_call(item));
                        block_origins
                            .response
                            .push(crate::codec::ir::IrBlockOrigin::at(
                                &origin::RESPONSE_WEB_SEARCH,
                            ));
                    }

                    other => {
                        crate::codec::drops::note(crate::codec::drops::Dropped::new(
                            format!("output[].type={other}"),
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            "dropping an unmodeled Responses output item on the cross-protocol \
                             seam: a hosted-tool invocation record (e.g. web_search_call) has no \
                             neutral IR form and no cross-protocol analog; the assistant \
                             message/function_call/reasoning output is unaffected",
                        ));
                    }
                }
            }
        } else {
            return Err(ir_parse_error());
        }

        if stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn)
            && content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. }))
        {
            stop_reason = Some(crate::codec::ir::IrStopReason::ToolUse);
        }

        if saw_refusal && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn) {
            stop_reason = Some(crate::codec::ir::IrStopReason::Refusal);
        }

        let usage_val = obj.get(keys::USAGE);

        let usage = read_responses_usage(usage_val, Some(body))?;

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        let id = obj.get(keys::ID).and_then(|i| i.as_str()).map(String::from);
        let created = obj.get(keys::CREATED_AT).and_then(|c| c.as_u64());

        if obj
            .get(keys::INSTRUCTIONS)
            .is_some_and(|v| !v.is_null() && v != &serde_json::json!(""))
        {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire(keys::INSTRUCTIONS),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping response `instructions` echo on Responses ir parse: IrResponse models no \
                 request-echo slot; the request-side instructions are carried on the request hop"
            );
        }
        if obj
            .get(keys::METADATA)
            .and_then(|m| m.as_object())
            .is_some_and(|m| !m.is_empty())
        {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire(keys::METADATA),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping response `metadata` echo on Responses ir parse: IrResponse models no \
                 request-echo slot; the request-side metadata is carried on the request hop"
            );
        }

        if obj
            .get(PREVIOUS_RESPONSE_ID)
            .is_some_and(|v| !v.is_null() && v != &serde_json::json!(""))
        {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire(PREVIOUS_RESPONSE_ID),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping response `previous_response_id` echo on Responses ir parse: IrResponse \
                 models no request-echo slot; the stateful linkage rides `response.id` (carried) \
                 and the request-side `previous_response_id` is carried on the request hop"
            );
        }
        if obj.get(FIELD_STORE).is_some_and(|v| !v.is_null()) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire(FIELD_STORE),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping response `store` echo on Responses ir parse: IrResponse models no \
                 request-echo slot; the request-side `store` flag is carried on the request hop"
            );
        }

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
            system_fingerprint: None,
            stop_sequence: None,

            request_echo: None,
            stop_detail: None,
            safety: super::read_moderation(body),
            ..Default::default()
        })
    }

    /// IR-18: a reasoning item's `encrypted_content` on the Responses wire is OpenAI-minted.
    fn stream_signature_origin(
        &self,
        _state: &crate::codec::ir::StreamDecodeState,
    ) -> Option<crate::codec::ir::IrSignatureOrigin> {
        Some(crate::codec::ir::IrSignatureOrigin::OpenAi)
    }

    /// Usage and terminal failures ride the terminal and error events; the reader reads the
    /// event name, never the payload's `type`.
    fn same_proto_reads(&self, event_type: &str, _payload: &[u8]) -> bool {
        matches!(
            event_type,
            EVT_RESPONSE_COMPLETED | EVT_RESPONSE_FAILED | EVT_RESPONSE_INCOMPLETE | "error"
        )
    }

    fn clone_box(&self) -> Box<dyn ProtocolReader> {
        Box::new(self.clone())
    }

    fn grammar(&self) -> &'static crate::codec::drops::Grammar {
        &GRAMMAR
    }
}

#[cfg(test)]
#[path = "tests/unreadable_count_refusal_tests.rs"]
mod unreadable_count_refusal_tests;
