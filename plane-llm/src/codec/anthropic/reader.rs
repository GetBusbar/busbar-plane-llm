use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;
use crate::codec::proto_codec::ReportedStreamUsage;

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

impl ProtocolReader for AnthropicReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;

        Some(usage::read_anthropic_usage(Some(&v)).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        let (provider_code, structured_type) =
            match crate::codec::json::parse::<serde_json::Value>(body) {
                Ok(json) => {
                    let error = json.get(keys::ERROR_WORD);
                    let provider_code = error
                        .and_then(|e| e.get(keys::CODE))
                        .and_then(|c| c.as_str())
                        .map(String::from);
                    let structured_type = error
                        .and_then(|e| e.get(keys::TYPE))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                    (provider_code, structured_type)
                }
                Err(_) => (None, None),
            };

        let status_code = status.as_u16();
        let is_request_size_status = status_code == 400 || status_code == 413;
        let provider_code = provider_code.or_else(|| {
            if !is_request_size_status {
                return None;
            }
            let lower = String::from_utf8_lossy(body).to_lowercase();
            if lower.contains("prompt is too long")
                || (lower.contains("exceeds the maximum")
                    && (lower.contains(keys::TOKEN) || lower.contains(keys::CONTEXT)))
            {
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
        crate::codec::dialect_classify::anthropic(status, body)
    }

    /// Usage rides only `message_start` and `message_delta`, and the terminal failure is `error`.
    fn same_proto_reads(&self, event_type: &str, _payload: &[u8]) -> bool {
        matches!(
            event_type,
            EVT_MESSAGE_START | EVT_MESSAGE_DELTA | keys::ERROR_WORD
        )
    }

    fn clone_box(&self) -> Box<dyn ProtocolReader> {
        Box::new(self.clone())
    }

    fn grammar(&self) -> &'static crate::codec::drops::Grammar {
        &GRAMMAR
    }

    /// IR-18: a `signature_delta` on the Anthropic wire is Claude's.
    fn stream_signature_origin(
        &self,
        _state: &crate::codec::ir::StreamDecodeState,
    ) -> Option<crate::codec::ir::IrSignatureOrigin> {
        Some(crate::codec::ir::IrSignatureOrigin::Anthropic)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        if obj.contains_key(ANTHROPIC_UNMODELED_BLOCKS_SENTINEL) {
            return Err(ir_parse_error());
        }

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();

        let mut system_turns_folded: usize = 0;

        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        if let Some(system_val) = obj.get(keys::SYSTEM) {
            if system_val.is_string() {
                let text = system_val.as_str().unwrap_or("").to_string();
                system_blocks.push(crate::codec::ir::IrBlock::Text {
                    text,
                    cache_control: None,
                    citations: Vec::new(),
                    refusal: false,
                });
                block_origins.system.push(string_origin(OriginSite::System));
            } else if let Some(arr) = system_val.as_array() {
                let (blocks, origins) = read_blocks_with_origins(arr, OriginSite::System)?;
                system_blocks.extend(blocks);
                block_origins.system.extend(origins);
            } else {
                return Err(ir_parse_error());
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();

        let mut unmodeled_blocks: Vec<serde_json::Value> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            let messages_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;
            for msg_val in messages_arr {
                let (msg, origins) = read_message_with_origins(msg_val)?;
                if msg.role == crate::codec::ir::IrRole::System {
                    system_turns_folded += 1;
                    let blocks_before = system_blocks.len();
                    system_blocks.extend(msg.content);
                    block_origins.system.extend(origins);
                    crate::codec::ir::IrSystemFold::record(
                        &mut system_folds,
                        messages.len(),
                        crate::codec::ir::IrSystemRole::System,
                        blocks_before,
                        system_blocks.len(),
                    );
                } else {
                    stash_unmodeled_blocks(msg_val, messages.len(), &mut unmodeled_blocks);
                    messages.push(msg);
                    block_origins.messages.push(origins);
                }
            }
        }
        if !unmodeled_blocks.is_empty() {
            extra.insert(
                ANTHROPIC_UNMODELED_BLOCKS_SENTINEL.to_string(),
                serde_json::Value::Array(unmodeled_blocks),
            );
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();

        let mut hosted_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_val) = obj.get(keys::TOOLS) {
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                super::slots::validate_hosted_tool(tool_val)?;
                match read_hosted_tool(tool_val) {
                    Some(hosted) => hosted_tools.push(hosted),
                    None => tools.push(read_tool(tool_val)?),
                }
            }
        }

        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0);
        if obj.get(keys::MAX_TOKENS).is_some_and(|value| {
            !value.is_null() && value.as_u64().and_then(|v| u32::try_from(v).ok()).is_none()
        }) {
            return Err(ir_parse_error());
        }

        let tool_choice = read_anthropic_tool_choice(obj.get(keys::TOOL_CHOICE));
        if obj
            .get(keys::TOOL_CHOICE)
            .is_some_and(|value| !value.is_null())
            && tool_choice.is_none()
        {
            return Err(ir_parse_error());
        }

        let parallel_tool_calls = obj
            .get(keys::TOOL_CHOICE)
            .and_then(|tc| tc.get(super::DISABLE_PARALLEL_TOOL_USE))
            .and_then(|v| v.as_bool())
            .map(|disabled| !disabled);
        if obj
            .get(keys::TOOL_CHOICE)
            .and_then(|tc| tc.get(super::DISABLE_PARALLEL_TOOL_USE))
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(ir_parse_error());
        }

        let user = obj
            .get(keys::METADATA)
            .and_then(|m| m.get(super::USER_ID))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let thinking_type = obj
            .get(keys::THINKING)
            .and_then(|t| t.get(keys::TYPE))
            .and_then(|v| v.as_str());
        let effort = obj
            .get(keys::OUTPUT_CONFIG)
            .and_then(|c| c.get(keys::EFFORT))
            .and_then(|v| v.as_str())
            .and_then(read_anthropic_effort_word);
        let reasoning = match thinking_type {
            Some(keys::ENABLED) => obj
                .get(keys::THINKING)
                .and_then(|t| t.get(keys::BUDGET_TOKENS))
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .map(crate::codec::ir::IrReasoningAsk::Budget),
            Some(THINKING_TYPE_ADAPTIVE) => Some(
                effort
                    .map(crate::codec::ir::IrReasoningAsk::Effort)
                    .unwrap_or(crate::codec::ir::IrReasoningAsk::Dynamic),
            ),
            Some(THINKING_TYPE_DISABLED) => Some(crate::codec::ir::IrReasoningAsk::Off),
            None => effort.map(crate::codec::ir::IrReasoningAsk::Effort),
            Some(_) => None,
        };
        let reasoning_is_budget =
            matches!(reasoning, Some(crate::codec::ir::IrReasoningAsk::Budget(_)));

        let output_format_legacy = obj.get(keys::OUTPUT_FORMAT);
        let response_format = obj
            .get(keys::OUTPUT_CONFIG)
            .and_then(|c| c.get(keys::FORMAT))
            .or(output_format_legacy)
            .and_then(read_anthropic_output_format);
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if obj
            .get(keys::STREAM)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(ir_parse_error());
        }

        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        if reasoning_is_budget {
            extra.remove(keys::THINKING);
        }

        if response_format.is_some() && output_format_legacy.is_some() {
            extra.remove(keys::OUTPUT_FORMAT);
        }

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            user,
            parallel_tool_calls,
            system: system_blocks,
            system_turns_folded,
            system_folds,
            messages,
            block_origins,
            tools,
            max_tokens,
            tool_choice,
            stream,
            response_format,
            extra,
            hosted_tools,
            ..Default::default()
        };
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        Ok(ir)
    }

    fn recover_response_usage(
        &self,
        body: &serde_json::Value,
    ) -> Option<busbar_contract::billing::TokenUsage> {
        let usage = body.get(keys::USAGE)?;
        Some(read_anthropic_usage(Some(usage)).ok()?.to_token_usage())
    }

    fn recover_stream_usage(
        &self,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Option<ReportedStreamUsage> {
        let (usage, at_start) = match event_type {
            EVT_MESSAGE_START => (data.get(keys::MESSAGE)?.get(keys::USAGE)?, true),
            EVT_MESSAGE_DELTA => (data.get(keys::USAGE)?, false),
            _ => return None,
        };
        if !usage.is_object() {
            return None;
        }
        let usage = read_anthropic_usage(Some(usage)).ok()?;
        Some(if at_start {
            ReportedStreamUsage::Start(usage)
        } else {
            ReportedStreamUsage::Trailing(usage)
        })
    }

    fn read_response_event(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Option<IrStreamEvent> {
        if matches!(
            event_type,
            EVT_CONTENT_BLOCK_START | EVT_CONTENT_BLOCK_DELTA | EVT_CONTENT_BLOCK_STOP
        ) && data
            .get(keys::INDEX)
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|index| index > MAX_ANTHROPIC_BLOCK_INDEX)
        {
            return Some(IrStreamEvent::Error(IrError {
                class: StatusClass::ServerError,
                provider_signal: Some(
                    "upstream content block index exceeds the supported range".into(),
                ),
                retry_after: None,
            }));
        }
        match event_type {
            EVT_MESSAGE_START => {
                let msg = data.get(keys::MESSAGE)?;
                let role_str = msg.get(keys::ROLE).and_then(|r| r.as_str())?;
                let role = match role_str {
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                    _ => return None,
                };

                let usage = match data
                    .get(keys::MESSAGE)
                    .and_then(|m| m.get(keys::USAGE))
                    .map(|u| read_anthropic_usage(Some(u)))
                    .transpose()
                {
                    Ok(usage) => usage,
                    Err(refusal) => return Some(IrStreamEvent::Error(refusal)),
                };

                let id = msg.get(keys::ID).and_then(|i| i.as_str()).map(String::from);

                let model = msg
                    .get(keys::MODEL)
                    .and_then(|m| m.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                Some(IrStreamEvent::MessageStart {
                    role,
                    usage,
                    id,
                    created: None,
                    model,
                })
            }
            EVT_CONTENT_BLOCK_START => {
                let index = read_clamped_block_index(data)?;
                let block = data.get(super::CONTENT_BLOCK)?;
                let block_type = block.get(keys::TYPE).and_then(|t| t.as_str())?;
                let meta = match block_type {
                    keys::TEXT => IrBlockMeta::Text,
                    keys::THINKING => IrBlockMeta::Thinking { kind: None },
                    STOP_TOOL_USE => {
                        let id = block
                            .get(keys::ID)
                            .and_then(|i| i.as_str())
                            .map(String::from)?;
                        let name = block
                            .get(keys::NAME)
                            .and_then(|n| n.as_str())
                            .map(String::from)?;
                        IrBlockMeta::ToolUse { id, name }
                    }
                    keys::IMAGE => IrBlockMeta::Image,
                    _ => return None,
                };
                Some(IrStreamEvent::BlockStart {
                    index,
                    block: meta,
                    refusal: false,
                })
            }
            EVT_CONTENT_BLOCK_DELTA => {
                let index = read_clamped_block_index(data)?;
                let delta_val = data.get(keys::DELTA)?;
                let delta_type = delta_val.get(keys::TYPE).and_then(|t| t.as_str())?;
                let delta = match delta_type {
                    DELTA_TYPE_TEXT => {
                        let text = delta_val
                            .get(keys::TEXT)
                            .and_then(|t| t.as_str())
                            .map(String::from)?;
                        IrDelta::TextDelta(text)
                    }
                    DELTA_TYPE_THINKING => {
                        let thinking = delta_val
                            .get(keys::THINKING)
                            .and_then(|t| t.as_str())
                            .map(String::from)?;
                        IrDelta::ThinkingDelta(thinking)
                    }
                    DELTA_TYPE_INPUT_JSON => {
                        let json = delta_val
                            .get(super::PARTIAL_JSON)
                            .or_else(|| delta_val.get("input_json"))
                            .and_then(|j| j.as_str())
                            .map(String::from)?;
                        IrDelta::InputJsonDelta(json)
                    }
                    DELTA_TYPE_SIGNATURE => {
                        let signature = delta_val
                            .get(keys::SIGNATURE)
                            .and_then(|s| s.as_str())
                            .map(String::from)?;
                        IrDelta::SignatureDelta(signature)
                    }

                    DELTA_TYPE_CITATIONS => {
                        let citation_val = delta_val.get(keys::CITATION)?;
                        IrDelta::CitationsDelta(vec![read_citation(citation_val)])
                    }
                    _ => return None,
                };
                Some(IrStreamEvent::BlockDelta { index, delta })
            }
            EVT_CONTENT_BLOCK_STOP => {
                let index = read_clamped_block_index(data)?;
                Some(IrStreamEvent::BlockStop { index })
            }
            EVT_MESSAGE_DELTA => {
                let delta = data.get(keys::DELTA)?;
                let stop_reason = delta
                    .get(super::STOP_REASON)
                    .and_then(|r| r.as_str())
                    .map(read_anthropic_stop_reason);

                let stop_sequence = delta
                    .get(keys::STOP_SEQUENCE)
                    .and_then(|s| s.as_str())
                    .map(String::from);

                let usage_val = data.get(keys::USAGE);
                let usage = match read_anthropic_usage(usage_val) {
                    Ok(usage) => usage,
                    Err(refusal) => return Some(IrStreamEvent::Error(refusal)),
                };

                let stop_detail = read_anthropic_stop_detail(
                    delta.get(super::STOP_REASON).and_then(|r| r.as_str()),
                    delta.get(super::STOP_DETAILS),
                );
                Some(IrStreamEvent::MessageDelta {
                    stop_reason,
                    stop_sequence,
                    usage,
                    stop_detail,
                })
            }
            EVT_MESSAGE_STOP => Some(IrStreamEvent::MessageStop),
            keys::ERROR_WORD => {
                let Some(err_val) = data.get(keys::ERROR_WORD) else {
                    return Some(IrStreamEvent::Error(IrError {
                        class: StatusClass::ServerError,
                        provider_signal: None,
                        retry_after: None,
                    }));
                };

                let type_token = err_val.get(keys::TYPE).and_then(|t| t.as_str());
                let provider_signal = type_token.map(String::from);

                let class = stream_error_class(type_token);
                Some(IrStreamEvent::Error(IrError {
                    class,
                    provider_signal,
                    retry_after: None,
                }))
            }
            _ => None,
        }
    }

    fn read_response_events(
        &self,
        host: &dyn crate::codec::cells::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        if matches!(
            event_type,
            EVT_CONTENT_BLOCK_START | EVT_CONTENT_BLOCK_DELTA | EVT_CONTENT_BLOCK_STOP
        ) && data
            .get(keys::INDEX)
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|index| index > MAX_ANTHROPIC_BLOCK_INDEX)
        {
            return self
                .read_response_event(host, event_type, data)
                .into_iter()
                .collect();
        }

        if event_type == EVT_CONTENT_BLOCK_START {
            let block_type = data
                .get(super::CONTENT_BLOCK)
                .and_then(|b| b.get(keys::TYPE))
                .and_then(|t| t.as_str());
            if let Some(index) = read_clamped_block_index(data) {
                state.open_tools.remove(&index);
                state.block_origins.remove(&index);
                let malformed_tool = block_type == Some(STOP_TOOL_USE)
                    && [keys::ID, keys::NAME].into_iter().any(|key| {
                        !data
                            .get(super::CONTENT_BLOCK)
                            .and_then(|block| block.get(key))
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                    });
                if block_type.is_none_or(|t| !is_streamed_anthropic_block_type(t)) || malformed_tool
                {
                    state.open_tools.insert(index);
                    if malformed_tool {
                        tracing::warn!(index, "dropping malformed upstream tool block and its deltas: missing tool identity");
                    }
                    return vec![];
                }
            }
        }
        if event_type == EVT_CONTENT_BLOCK_DELTA || event_type == EVT_CONTENT_BLOCK_STOP {
            if let Some(index) = read_clamped_block_index(data) {
                if state.open_tools.contains(&index) {
                    if event_type == EVT_CONTENT_BLOCK_STOP {
                        state.open_tools.remove(&index);
                    }
                    return vec![];
                }
            }
        }

        if event_type == EVT_CONTENT_BLOCK_START {
            if let Some(block) = data.get(super::CONTENT_BLOCK) {
                if block.get(keys::TYPE).and_then(|t| t.as_str())
                    == Some(BLOCK_TYPE_REDACTED_THINKING)
                {
                    if let Some(index) = read_clamped_block_index(data) {
                        let bytes = block
                            .get(keys::DATA)
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string();
                        if let Some(origin) = stream_block_origin(BLOCK_TYPE_REDACTED_THINKING) {
                            state.block_origins.insert(index, origin);
                        }
                        return vec![
                            // A redacted start: the writer must know at block-start time to emit
                            // `redacted_thinking`, never a plaintext `thinking` seed.
                            IrStreamEvent::BlockStart {
                                index,
                                block: IrBlockMeta::RedactedThinking,
                                refusal: false,
                            },
                            IrStreamEvent::BlockDelta {
                                index,
                                delta: IrDelta::RedactedReasoningDelta(bytes),
                            },
                        ];
                    }
                }
            }
        }

        match self.read_response_event(host, event_type, data) {
            Some(ev) => {
                if let IrStreamEvent::BlockStart { index, .. } = &ev {
                    if let Some(kind) = data
                        .get(super::CONTENT_BLOCK)
                        .and_then(|block| block.get(keys::TYPE))
                        .and_then(|kind| kind.as_str())
                    {
                        if let Some(origin) = stream_block_origin(kind) {
                            state.block_origins.insert(*index, origin);
                        }
                    }
                }
                vec![ev]
            }
            None => vec![],
        }
    }

    fn read_response(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let role_str = obj.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");
        let role = match role_str {
            keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
            _ => return Err(ir_parse_error()),
        };

        let content_val = obj.get(keys::CONTENT).ok_or_else(ir_parse_error)?;
        if !content_val.is_array() {
            return Err(ir_parse_error());
        }
        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();
        if let Some(arr) = content_val.as_array() {
            for block_val in arr
                .iter()
                .filter(|b| super::RESPONSE_BLOCKS.iter().all(|g| g.models(b)))
            {
                let (block, origin) = read_block_with_origin(block_val, OriginSite::Response)?;
                content.push(block);
                block_origins.response.push(origin);
            }
        }

        let stop_reason = obj
            .get(super::STOP_REASON)
            .and_then(|r| r.as_str())
            .map(read_anthropic_stop_reason);

        let usage_val = obj.get(keys::USAGE);

        let usage = read_anthropic_usage(usage_val)?;

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let id = obj.get(keys::ID).and_then(|i| i.as_str()).map(String::from);

        let stop_sequence = obj
            .get(keys::STOP_SEQUENCE)
            .and_then(|s| s.as_str())
            .map(String::from);

        Ok(crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role,
            content,
            block_origins,
            stop_reason,
            usage,
            model,
            id,
            created: None,
            system_fingerprint: None,
            stop_sequence,

            request_echo: None,

            stop_detail: read_anthropic_stop_detail(
                obj.get(super::STOP_REASON).and_then(|r| r.as_str()),
                obj.get(super::STOP_DETAILS),
            ),
            ..Default::default()
        })
    }
}

#[cfg(test)]
#[path = "tests/unreadable_count_refusal_tests.rs"]
mod unreadable_count_refusal_tests;

#[cfg(test)]
#[path = "tests/reader_block_origin_tests.rs"]
mod block_origin_tests;
