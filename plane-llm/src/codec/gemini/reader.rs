const INITIAL_THINKING_INDEX: usize = 0;

use crate::codec::ir::{IrBlockOrigin, IrBlockOrigins, IrBlockSource};

static SYSTEM_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "systemInstruction.parts[]",
};
static REQUEST_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "contents[].parts[]",
};
static FUNCTION_RESPONSE_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "contents[].parts[].functionResponse.response",
};
static FUNCTION_RESPONSE_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "contents[].parts[].functionResponse.parts[]",
};
static RESPONSE_PART_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "candidates[].content.parts[]",
};
static GROUNDING_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "candidates[].groundingMetadata.groundingChunks[]",
};
static CITATION_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "candidates[]",
};
static LOGPROBS_SOURCE: IrBlockSource = IrBlockSource {
    dialect: COUNT_LABEL,
    path: "candidates[].logprobsResult",
};

use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;

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
        map: super::map::RESPONSE_PATHS,
        code: super::RESPONSE_CODE,
        drops: super::RESPONSE_DROPS,
        defaults: &[],
        reroot: None,
    }),
    stream_keyed_by_event: false,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for GeminiReader {
    /// A candidate's own `finishReason` member (`{"candidates":[{…,"finishReason":…}]}`, depth 3);
    /// the first is the first candidate's, which `read_response` reads.
    fn stop_reason_key(&self) -> Option<crate::codec::usage_tail::StopKey> {
        Some(crate::codec::usage_tail::StopKey {
            key: b"\"finishReason\"",
            depth: 3,
        })
    }

    /// The same `map_gemini_finish_reason` `read_response` maps it through.
    fn stop_reason_of_token(&self, token: &str) -> Option<crate::codec::ir::IrStopReason> {
        Some(map_gemini_finish_reason(token))
    }

    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usageMetadata\"")?;

        let mut usage = crate::codec::usage_count::read_usage(COUNT_LABEL, Some(&v), USAGE).ok()?;
        usage.detail.usage_identity_note = crate::codec::usage_count::stated_total_note(
            COUNT_LABEL,
            GEMINI_USAGE_IDENTITY,
            v.get(FIELD_TOTAL_TOKEN_COUNT),
            &mut usage,
        );
        Some(usage.to_token_usage())
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
            .and_then(|c| {
                c.as_u64()
                    .map(|n| n.to_string())
                    .or_else(|| c.as_str().map(String::from))
            })
            .or_else(|| {
                error_obj
                    .and_then(|e_obj| e_obj.get(keys::STATUS))
                    .and_then(|s| s.as_str())
                    .map(String::from)
            });

        let structured_type = error_obj
            .and_then(|e_obj| e_obj.get(keys::STATUS))
            .and_then(|t| t.as_str())
            .map(String::from);

        let provider_code = {
            let st = status.as_u16();
            if st == 400 || st == 413 {
                let lower = String::from_utf8_lossy(body).to_lowercase();
                if lower.contains("input is longer than the maximum number of tokens")
                    || (lower.contains(keys::MAXIMUM_TOKENS) && lower.contains(keys::REQUESTED))
                    || (lower.contains("token count")
                        && (lower.contains("exceeds") || lower.contains("exceed"))
                        && lower.contains("maximum"))
                    || (lower.contains("exceeds the maximum")
                        && (lower.contains(keys::TOKEN) || lower.contains("context")))
                {
                    Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
                } else {
                    provider_code
                }
            } else {
                provider_code
            }
        };

        let (http_status, provider_code) = {
            let has_api_key_invalid_reason = error_obj
                .and_then(|e_obj| e_obj.get(FIELD_DETAILS))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|details| {
                    details.iter().any(|d| {
                        d.get("@type").and_then(serde_json::Value::as_str)
                            == Some(GEMINI_ERROR_INFO_TYPE_URL)
                            && d.get(keys::REASON)
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|r| {
                                    r.eq_ignore_ascii_case(GEMINI_ERROR_REASON_API_KEY_INVALID)
                                })
                    })
                });

            let api_key_message = error_obj
                .and_then(|error| error.get(keys::MESSAGE))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| {
                    message.eq_ignore_ascii_case(GEMINI_BAD_KEY_MESSAGE)
                        || message
                            .eq_ignore_ascii_case("API key expired. Please renew the API key.")
                });

            let status_is_auth_shaped = matches!(
                structured_type.as_deref(),
                Some(GRPC_INVALID_ARGUMENT)
                    | Some(GRPC_PERMISSION_DENIED)
                    | Some(GRPC_UNAUTHENTICATED)
            );
            if has_api_key_invalid_reason || (status_is_auth_shaped && api_key_message) {
                (401u16, Some(keys::AUTH_WORD.to_string()))
            } else {
                (status.as_u16(), provider_code)
            }
        };

        busbar_contract::upstream::RawUpstreamError {
            http_status,
            provider_code,
            structured_type,
            retry_after_secs: None,
        }
    }

    #[cfg(test)]
    fn classify(&self, status: StatusCode, body: &[u8]) -> CanonicalSignal {
        crate::codec::dialect_classify::gemini(status, body)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = IrBlockOrigins::default();

        let mut tool_call_index: usize = 0;

        let mut call_ledger = GeminiCallLedger::default();

        if let Some(sys_instr) = obj.get(FIELD_SYSTEM_INSTRUCTION) {
            if let Some(parts_arr) = sys_instr.get(FIELD_PARTS).and_then(|p| p.as_array()) {
                for part in parts_arr {
                    if let Some(text_val) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                        system_blocks.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                        block_origins
                            .system
                            .push(IrBlockOrigin::at(&SYSTEM_PART_SOURCE));
                    }
                }
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(contents_val) = obj.get(FIELD_CONTENTS) {
            let contents_arr = contents_val.as_array().ok_or_else(ir_parse_error)?;
            for (turn, content_val) in contents_arr.iter().enumerate() {
                let role_str = content_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let role = match role_str {
                    keys::USER | "" => crate::codec::ir::IrRole::User,
                    keys::MODEL => crate::codec::ir::IrRole::Assistant,
                    _ => return Err(ir_parse_error()),
                };

                let mut msg_content = Vec::new();
                let mut msg_origins = Vec::new();

                let parts_val = content_val.get(FIELD_PARTS);
                if let Some(pv) = parts_val {
                    if !pv.is_array() {
                        return Err(ir_parse_error());
                    }
                }
                if let Some(parts_arr) = parts_val.and_then(|p| p.as_array()) {
                    for part in parts_arr {
                        if part.get(FIELD_THOUGHT).and_then(|t| t.as_bool()) == Some(true) {
                            let text = part
                                .get(keys::TEXT)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            let signature = part
                                .get(FIELD_THOUGHT_SIGNATURE)
                                .and_then(|s| s.as_str())
                                .map(String::from);

                            let (signature, signature_origin) =
                                crate::codec::ir::sig_envelope::read_carried_opt(signature, None);
                            msg_content.push(crate::codec::ir::IrBlock::Thinking {
                                text,
                                signature,
                                redacted: false,
                                cache_control: None,
                                kind: Some(crate::codec::ir::IrThinkingKind::Summary),
                                signature_origin,
                            });
                            msg_origins.push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                        } else if let Some(text_val) = part.get(keys::TEXT).and_then(|t| t.as_str())
                        {
                            msg_content.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                            msg_origins.push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                        } else if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                            let name = func_call
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();

                            let args = empty_object_if_absent(func_call.get(FIELD_ARGS));

                            let id = gemini_call_id(func_call)
                                .map(str::to_string)
                                .unwrap_or_else(|| synth_tool_call_id(tool_call_index, &name, ""));
                            tool_call_index += 1;
                            call_ledger.record_call(&id, &name, turn);

                            let thought_signature = part
                                .get(FIELD_THOUGHT_SIGNATURE)
                                .and_then(|s| s.as_str())
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                            msg_content.push(crate::codec::ir::IrBlock::ToolUse {
                                id,
                                name,
                                input: args,
                                cache_control: None,
                                thought_signature,
                            });
                            msg_origins.push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                        } else if let Some(func_resp) = part.get(FIELD_FUNCTION_RESPONSE) {
                            let name = func_resp
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let response_val = func_resp
                                .get(keys::RESPONSE)
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);

                            let response_text = crate::codec::json::to_string(&response_val)
                                .unwrap_or_else(|_| "unknown".to_string());

                            let tool_use_id =
                                call_ledger.pair_response(gemini_call_id(func_resp), &name);

                            let is_error = response_val.as_object().is_some_and(|o| {
                                o.get(keys::ERROR_WORD).is_some_and(|e| !e.is_null())
                                    && !o.contains_key(keys::OUTPUT)
                            });
                            let mut content = vec![crate::codec::ir::IrBlock::text(response_text)];
                            let response_origin = if func_resp.get(keys::RESPONSE).is_some() {
                                IrBlockOrigin::at(&FUNCTION_RESPONSE_SOURCE)
                            } else {
                                IrBlockOrigin::default()
                            };
                            let mut child_origins = vec![response_origin];

                            if let Some(parts) =
                                func_resp.get(FIELD_PARTS).and_then(|p| p.as_array())
                            {
                                for part in parts {
                                    if let Some(block) = read_gemini_media_part(part) {
                                        content.push(block);
                                        child_origins.push(IrBlockOrigin::at(
                                            &FUNCTION_RESPONSE_PART_SOURCE,
                                        ));
                                    }
                                }
                            }
                            msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                                cache_control: None,
                            });
                            msg_origins.push(IrBlockOrigin {
                                source: Some(&REQUEST_PART_SOURCE),
                                children: child_origins,
                            });
                        } else if let Some(block) = read_gemini_media_part(part) {
                            msg_content.push(block);
                            msg_origins.push(IrBlockOrigin::at(&REQUEST_PART_SOURCE));
                        }
                    }
                }

                messages.push(crate::codec::ir::IrMessage {
                    role,
                    content: msg_content,
                });
                block_origins.messages.push(msg_origins);
            }
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        let mut hosted_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_arr) = obj.get(keys::TOOLS).and_then(|t| t.as_array()) {
            for tool_val in tools_arr {
                if let Some(func_decls) = tool_val
                    .get(FIELD_FUNCTION_DECLARATIONS)
                    .and_then(|f| f.as_array())
                {
                    for func_decl in func_decls {
                        let name = func_decl
                            .get(keys::NAME)
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let description = func_decl
                            .get(keys::DESCRIPTION)
                            .and_then(|d| d.as_str().map(String::from));

                        let parameters = func_decl
                            .get("parametersJsonSchema")
                            .cloned()
                            .or_else(|| {
                                func_decl
                                    .get(keys::PARAMETERS)
                                    .map(gemini_openapi_schema_to_json_schema)
                            })
                            .unwrap_or(serde_json::Value::Null);

                        tools.push(crate::codec::ir::IrTool {
                            name,
                            description,
                            input_schema: parameters,
                            cache_control: None,
                            hosted: None,
                            strict: None,
                        });
                    }
                }

                let (typed, raw) = read_gemini_hosted_tools(tool_val);
                hosted_tools.extend(typed);
                tools.extend(raw);
            }
        }

        let max_tokens = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_MAX_OUTPUT_TOKENS))
            .and_then(|v| v.as_i64())
            .filter(|&v| v > 0)
            .and_then(|v| u32::try_from(v).ok());

        let response_format = read_gemini_response_format(obj.get(FIELD_GENERATION_CONFIG));

        let logprobs = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_RESPONSE_LOGPROBS))
            .and_then(|v| v.as_bool());
        let top_logprobs = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(keys::LOGPROBS))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let thinking_config = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_THINKING_CONFIG));
        let reasoning = match thinking_config
            .and_then(|tc| tc.get(FIELD_THINKING_BUDGET))
            .filter(|b| !b.is_null())
        {
            Some(budget) => match read_gemini_thinking_budget(budget).ok_or_else(ir_parse_error)? {
                -1 => Some(crate::codec::ir::IrReasoningAsk::Dynamic),

                0 => Some(crate::codec::ir::IrReasoningAsk::Off),
                n => Some(crate::codec::ir::IrReasoningAsk::Budget(n.unsigned_abs())),
            },

            None => thinking_config
                .and_then(|tc| tc.get("thinkingLevel"))
                .and_then(|v| v.as_str())
                .and_then(read_gemini_thinking_level),
        };
        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

        let tool_choice = read_gemini_tool_choice(obj.get(keys::TOOL_CONFIG));

        let allowed_tools = read_gemini_allowed_tools(obj.get(keys::TOOL_CONFIG));

        if let Some(model_val) = obj.get(keys::MODEL) {
            extra.insert(keys::MODEL.to_string(), model_val.clone());
        }

        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        let mut ir = crate::codec::ir::IrRequest {
            block_origins,
            reasoning,
            logprobs,
            top_logprobs,
            system: system_blocks,
            messages,
            tools,
            max_tokens,
            tool_choice,
            stream,
            response_format,
            extra,

            metadata: read_gemini_labels(obj.get(FIELD_LABELS)),
            allowed_tools,
            hosted_tools,

            output_modalities: read_gemini_response_modalities(obj.get(FIELD_GENERATION_CONFIG)),
            ..Default::default()
        };
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        Ok(ir)
    }

    fn read_response_events(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        _event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();

        if data.as_str() == Some(crate::codec::dialect::SSE_DONE_SENTINEL) || !data.is_object() {
            return out;
        }

        if let Some(error_obj) = data.get(keys::ERROR_WORD).and_then(|e| e.as_object()) {
            let status_str = error_obj.get(keys::STATUS).and_then(|s| s.as_str());
            let code = error_obj.get(keys::CODE).and_then(|c| c.as_u64());
            let class = gemini_error_status_class(status_str, code);
            let message = error_obj
                .get(keys::MESSAGE)
                .and_then(|m| m.as_str())
                .map(String::from)
                .or_else(|| status_str.map(String::from));
            out.push(IrStreamEvent::Error(crate::codec::protocol::IrError {
                class,
                provider_signal: message,
                retry_after: None,
            }));
            return out;
        }

        if candidates_absent(data) {
            if let Some(block_reason) = prompt_block_reason(data) {
                if !state.started {
                    state.started = true;
                    let id = data
                        .get(FIELD_RESPONSE_ID)
                        .and_then(|i| i.as_str())
                        .map(String::from);
                    let model = data
                        .get(FIELD_MODEL_VERSION)
                        .or_else(|| data.get(keys::MODEL))
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    out.push(IrStreamEvent::MessageStart {
                        role: crate::codec::ir::IrRole::Assistant,
                        usage: None,
                        id,
                        created: None,
                        model,
                    });
                }

                close_open_blocks(state, &mut out);

                let usage = match read_gemini_usage(data) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };
                out.push(IrStreamEvent::MessageDelta {
                    stop_reason: Some(prompt_block_stop_reason(block_reason)),
                    stop_sequence: None,
                    usage,
                    stop_detail: None,
                });
                out.push(IrStreamEvent::MessageStop);
                return out;
            }
        }

        if !state.started {
            state.started = true;
            let id = data
                .get(FIELD_RESPONSE_ID)
                .and_then(|i| i.as_str())
                .map(String::from);
            let model = data
                .get(FIELD_MODEL_VERSION)
                .or_else(|| data.get(keys::MODEL))
                .and_then(|m| m.as_str())
                .map(String::from);
            out.push(IrStreamEvent::MessageStart {
                role: crate::codec::ir::IrRole::Assistant,
                usage: None,
                id,
                created: None,
                model,
            });
        }

        let candidates = data.get(FIELD_CANDIDATES).and_then(|c| c.as_array());

        if let Some(cands) = candidates {
            if cands.len() > 1 && !state.multi_candidate_warned {
                state.multi_candidate_warned = true;
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire("candidates[]"),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [candidates = cands.len(),],
                    "gemini stream chunk carried multiple candidates; only candidates[0] survives IR translation — a cross-protocol hop drops the rest (a same-protocol relay preserves all)"
                );
            }
        }

        if let Some(candidate) = candidates.and_then(|cands| cands.first()) {
            if let Some(content) = candidate.get(keys::CONTENT) {
                let role_val = content
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");

                if role_val == keys::MODEL || role_val.is_empty() {
                    if let Some(parts_arr) = content.get(FIELD_PARTS).and_then(|p| p.as_array()) {
                        for (part_index, part) in parts_arr.iter().enumerate() {
                            if part.get(FIELD_THOUGHT).and_then(|t| t.as_bool()) == Some(true) {
                                let text = part
                                    .get(keys::TEXT)
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default();
                                let signature =
                                    part.get(FIELD_THOUGHT_SIGNATURE).and_then(|v| v.as_str());
                                if (!text.is_empty() || signature.is_some_and(|s| !s.is_empty()))
                                    && !state.text_block_open
                                    && state.open_tools.is_empty()
                                    && state.next_ir_index == 0
                                {
                                    state.reasoning_seen = true;
                                    if !state.thinking_block_open {
                                        state.thinking_block_open = true;
                                        state.block_origins.insert(
                                            INITIAL_THINKING_INDEX,
                                            IrBlockOrigin::at(&RESPONSE_PART_SOURCE),
                                        );
                                        out.push(IrStreamEvent::BlockStart {
                                            index: INITIAL_THINKING_INDEX,
                                            block: crate::codec::ir::IrBlockMeta::Thinking {
                                                kind: Some(
                                                    crate::codec::ir::IrThinkingKind::Summary,
                                                ),
                                            },
                                            refusal: false,
                                        });
                                    }
                                    if !text.is_empty() {
                                        out.push(IrStreamEvent::BlockDelta {
                                            index: 0,
                                            delta: crate::codec::ir::IrDelta::ThinkingDelta(
                                                text.to_string(),
                                            ),
                                        });
                                    }
                                    if let Some(sig) = signature {
                                        out.push(IrStreamEvent::BlockDelta {
                                            index: 0,
                                            delta: crate::codec::ir::IrDelta::SignatureDelta(
                                                sig.to_string(),
                                            ),
                                        });
                                    }
                                }
                                continue;
                            }

                            if let Some(text) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    let ti = match state.text_index {
                                        Some(ti) if !state.text_block_closed => ti,
                                        _ => {
                                            state.text_block_start = state.streamed_chars;
                                            match state.claim_ir_index() {
                                                Ok(index) => index,
                                                Err(error) => {
                                                    return vec![IrStreamEvent::Error(error)];
                                                }
                                            }
                                        }
                                    };
                                    state.text_block_closed = false;
                                    if state.thinking_block_open {
                                        state.thinking_block_open = false;
                                        out.push(IrStreamEvent::BlockStop { index: 0 });
                                    }
                                    if !state.text_block_open {
                                        state.text_block_open = true;
                                        state.text_index = Some(ti);
                                        state
                                            .block_origins
                                            .insert(ti, IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                                        out.push(IrStreamEvent::BlockStart {
                                            index: ti,
                                            block: crate::codec::ir::IrBlockMeta::Text,
                                            refusal: false,
                                        });
                                    }

                                    if state.streamed_text.len().saturating_add(text.len())
                                        <= crate::codec::cells::TRANSLATE_BODY_MAX_BYTES_DEFAULT
                                    {
                                        let answer_start = state.streamed_text.len();
                                        let char_start = state.streamed_chars;
                                        extend_char_marks(
                                            &mut state.streamed_char_marks,
                                            &mut state.streamed_chars,
                                            state.streamed_text.len(),
                                            text,
                                        );
                                        state.streamed_text.push_str(text);
                                        state.gemini_citations.record_part(
                                            part_index,
                                            (answer_start, char_start),
                                            (ti, state.text_block_start),
                                            (text.len(), state.streamed_chars - char_start),
                                        );
                                    }
                                    out.push(IrStreamEvent::BlockDelta {
                                        index: ti,
                                        delta: crate::codec::ir::IrDelta::TextDelta(
                                            text.to_string(),
                                        ),
                                    });
                                }
                            }

                            if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                                let name_val = func_call
                                    .get(keys::NAME)
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                if !name_val.is_empty()
                                    && state.open_tools.len() >= MAX_GEMINI_TOOL_FRAMES
                                    && !state.tool_frame_cap_warned
                                {
                                    state.tool_frame_cap_warned = true;
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::wire(
                                            "candidates[].content.parts[].functionCall"
                                        ),
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [cap = MAX_GEMINI_TOOL_FRAMES,],
                                        "gemini stream exceeded MAX_GEMINI_TOOL_FRAMES concurrent tool-call frames; new functionCall parts are being dropped for the rest of this stream"
                                    );
                                }

                                if !name_val.is_empty()
                                    && state.open_tools.len() < MAX_GEMINI_TOOL_FRAMES
                                {
                                    let tool_call_index = state.open_tools.len();
                                    let ir_idx = match state.claim_ir_index() {
                                        Ok(index) => index,
                                        Err(error) => return vec![IrStreamEvent::Error(error)],
                                    };
                                    state.open_tools.insert(ir_idx);

                                    if state.thinking_block_open {
                                        state.thinking_block_open = false;
                                        out.push(IrStreamEvent::BlockStop { index: 0 });
                                    }

                                    if state.text_block_open {
                                        state.text_block_open = false;
                                        state.text_block_closed = true;
                                        let ti = state.text_index.unwrap_or(0);
                                        out.push(IrStreamEvent::BlockStop { index: ti });
                                    }

                                    let args = empty_object_if_absent(func_call.get(FIELD_ARGS));

                                    let turn_salt =
                                        data.get(FIELD_RESPONSE_ID).and_then(|v| v.as_str());

                                    let id = gemini_call_id(func_call)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            synth_tool_call_id(
                                                tool_call_index,
                                                &name_val,
                                                turn_salt.unwrap_or(""),
                                            )
                                        });
                                    state
                                        .block_origins
                                        .insert(ir_idx, IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                                    out.push(IrStreamEvent::BlockStart {
                                        index: ir_idx,
                                        block: crate::codec::ir::IrBlockMeta::ToolUse {
                                            id,
                                            name: name_val.clone(),
                                        },
                                        refusal: false,
                                    });

                                    let args_str =
                                        crate::codec::json::to_string(&args).unwrap_or_default();
                                    out.push(IrStreamEvent::BlockDelta {
                                        index: ir_idx,
                                        delta: crate::codec::ir::IrDelta::InputJsonDelta(args_str),
                                    });
                                }
                            }

                            if let Some(block) = read_gemini_media_part(part) {
                                if state.thinking_block_open {
                                    state.thinking_block_open = false;
                                    out.push(IrStreamEvent::BlockStop { index: 0 });
                                }
                                if state.text_block_open {
                                    state.text_block_open = false;
                                    state.text_block_closed = true;
                                    out.push(IrStreamEvent::BlockStop {
                                        index: state.text_index.unwrap_or(0),
                                    });
                                }
                                let index = match state.claim_ir_index() {
                                    Ok(index) => index,
                                    Err(error) => return vec![IrStreamEvent::Error(error)],
                                };
                                state
                                    .block_origins
                                    .insert(index, IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                                out.push(IrStreamEvent::BlockStart {
                                    index,
                                    block: crate::codec::ir::IrBlockMeta::Image,
                                    refusal: false,
                                });
                                out.push(IrStreamEvent::BlockDelta {
                                    index,
                                    delta: crate::codec::ir::IrDelta::MediaDelta(Box::new(block)),
                                });
                                out.push(IrStreamEvent::BlockStop { index });
                            }
                        }
                    }
                }
            }

            let all_citations = match read_gemini_streamed_citations(
                candidate,
                &state.streamed_text,
                &state.streamed_char_marks,
                &mut state.gemini_citations,
                &mut state.citations_emitted,
            ) {
                Ok(citations) => citations,
                Err(citations) => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::wire("candidates[].groundingMetadata"),
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "grounding metadata exceeded the existing citation retention bound; \
                         unretainable citations are omitted without changing text or usage"
                    );
                    citations
                }
            };
            let mut by_block: std::collections::BTreeMap<usize, Vec<crate::codec::ir::IrCitation>> =
                std::collections::BTreeMap::new();
            for mut c in all_citations {
                let (target, start) = c
                    .start_index
                    .and_then(|start| state.gemini_citations.owner(start))
                    .unwrap_or((
                        state.text_index.unwrap_or(usize::MAX),
                        state.text_block_start,
                    ));
                let block_start = i64::try_from(start).unwrap_or(i64::MAX);
                if block_start > 0 {
                    c.start_index = c.start_index.map(|s| (s - block_start).max(0));
                    c.end_index = c.end_index.map(|e| (e - block_start).max(0));
                }
                by_block.entry(target).or_default().push(c);
            }
            for (target, citations) in by_block {
                if target != usize::MAX && state.text_index != Some(target) {
                    out.push(IrStreamEvent::BlockDelta {
                        index: target,
                        delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                    });
                    continue;
                }
                if !citations.is_empty() && state.text_block_closed && !state.text_block_open {
                    if let Some(ti) = state.text_index {
                        out.push(IrStreamEvent::BlockDelta {
                            index: ti,
                            delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                        });
                    }
                } else if !citations.is_empty() {
                    let ti = match state.text_index {
                        Some(index) => index,
                        None => match state.claim_ir_index() {
                            Ok(index) => index,
                            Err(error) => return vec![IrStreamEvent::Error(error)],
                        },
                    };
                    if !state.text_block_open {
                        if state.thinking_block_open {
                            state.thinking_block_open = false;
                            out.push(IrStreamEvent::BlockStop { index: 0 });
                        }
                        state.text_block_open = true;
                        state.text_index = Some(ti);
                        state
                            .block_origins
                            .insert(ti, IrBlockOrigin::at(&CITATION_SOURCE));
                        out.push(IrStreamEvent::BlockStart {
                            index: ti,
                            block: crate::codec::ir::IrBlockMeta::Text,
                            refusal: false,
                        });
                    }
                    out.push(IrStreamEvent::BlockDelta {
                        index: ti,
                        delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                    });
                }
            }

            let stream_logprobs = read_gemini_logprobs(candidate.get(FIELD_LOGPROBS_RESULT));
            if !stream_logprobs.is_empty() {
                let ti = match state.text_index {
                    Some(ti) if !state.text_block_closed => ti,
                    _ => match state.claim_ir_index() {
                        Ok(index) => index,
                        Err(error) => return vec![IrStreamEvent::Error(error)],
                    },
                };
                if !state.text_block_open {
                    state.text_block_closed = false;

                    if state.thinking_block_open {
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: 0 });
                    }
                    state.text_block_open = true;
                    state.text_index = Some(ti);
                    state
                        .block_origins
                        .insert(ti, IrBlockOrigin::at(&LOGPROBS_SOURCE));
                    out.push(IrStreamEvent::BlockStart {
                        index: ti,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
                out.push(IrStreamEvent::BlockDelta {
                    index: ti,
                    delta: crate::codec::ir::IrDelta::LogprobsDelta(stream_logprobs),
                });
            }

            if let Some(finish_reason_val) =
                candidate.get(FIELD_FINISH_REASON).and_then(|r| r.as_str())
            {
                let mut stop_reason = map_gemini_finish_reason(finish_reason_val);

                if stop_reason == crate::codec::ir::IrStopReason::EndTurn
                    && !state.open_tools.is_empty()
                {
                    stop_reason = crate::codec::ir::IrStopReason::ToolUse;
                }

                close_open_blocks(state, &mut out);

                let usage = match read_gemini_usage(data) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };

                if stop_reason == crate::codec::ir::IrStopReason::Error {
                    out.push(IrStreamEvent::Error(IrError {
                        class: busbar_contract::upstream::StatusClass::ServerError,
                        provider_signal: Some(finish_reason_val.to_string()),
                        retry_after: None,
                    }));
                }

                out.push(IrStreamEvent::MessageDelta {
                    stop_reason: Some(stop_reason),

                    stop_sequence: None,
                    usage,
                    stop_detail: None,
                });
                out.push(IrStreamEvent::MessageStop);
            }
        }

        out
    }

    fn read_response(
        &self,
        _host: &dyn crate::codec::cells::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        // Prompt-blocked envelope. The prompt itself can be rejected: a body with a top-level
        // `promptFeedback.blockReason` (SAFETY/BLOCKLIST/PROHIBITED_CONTENT/OTHER), no `candidates` and
        // no `error`. Hard-failing the absent-candidates path below turned this content-policy block into
        // a spurious `ir_parse` ClientError with no surfaced reason. Return an empty-content response
        // with the mapped stop reason, mirroring the streaming prompt-block sequence and the
        // SAFETY-filtered-candidate tolerance below. Usage is surfaced when present.
        if candidates_absent(body) {
            if let Some(block_reason) = prompt_block_reason(body) {
                let usage = read_gemini_usage(body)?;
                let model = obj
                    .get(FIELD_MODEL_VERSION)
                    .or_else(|| obj.get(keys::MODEL))
                    .and_then(|m| m.as_str())
                    .map(String::from);
                let id = obj
                    .get(FIELD_RESPONSE_ID)
                    .and_then(|i| i.as_str())
                    .map(String::from);
                return Ok(crate::codec::ir::IrResponse {
                    logprobs: Vec::new(),
                    role: crate::codec::ir::IrRole::Assistant,
                    content: Vec::new(),
                    stop_reason: Some(prompt_block_stop_reason(block_reason)),
                    usage,
                    model,
                    id,
                    created: None,
                    system_fingerprint: None,
                    stop_sequence: None,
                    safety: super::read_safety_ratings(body, &serde_json::Value::Null),

                    request_echo: None,
                    stop_detail: None,
                    ..Default::default()
                });
            }
        }

        let candidates_val = obj.get(FIELD_CANDIDATES).ok_or_else(ir_parse_error)?;
        let candidates = candidates_val.as_array().ok_or_else(ir_parse_error)?;

        if candidates.is_empty() {
            return Err(ir_parse_error());
        }

        if candidates.len() > 1 {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("candidates[]"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [candidates = candidates.len(),],
                "gemini response carried multiple candidates; only the first is translated cross-protocol (the rest are dropped)"
            );
        }

        let candidate = &candidates[0];

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = IrBlockOrigins::default();

        if let Some(block) = super::citations::read_grounding_record(candidate) {
            content.push(block);
            block_origins
                .response
                .push(IrBlockOrigin::at(&GROUNDING_SOURCE));
        }

        let mut tool_call_index: usize = 0;

        let tool_id_turn_salt = obj
            .get(FIELD_RESPONSE_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Some(parts_arr) = candidate
            .get(keys::CONTENT)
            .and_then(|c| c.get(FIELD_PARTS))
            .and_then(|p| p.as_array())
        {
            for part in parts_arr {
                if part.get(FIELD_THOUGHT).and_then(|t| t.as_bool()) == Some(true) {
                    let text = part
                        .get(keys::TEXT)
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let signature = part
                        .get(FIELD_THOUGHT_SIGNATURE)
                        .and_then(|s| s.as_str())
                        .map(String::from);

                    let (signature, signature_origin) =
                        crate::codec::ir::sig_envelope::read_carried_opt(
                            signature,
                            Some(crate::codec::ir::IrSignatureOrigin::Gemini),
                        );
                    content.push(crate::codec::ir::IrBlock::Thinking {
                        text,
                        signature,
                        redacted: false,
                        cache_control: None,
                        kind: Some(crate::codec::ir::IrThinkingKind::Summary),
                        signature_origin,
                    });
                    block_origins
                        .response
                        .push(IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                } else if let Some(text) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        content.push(crate::codec::ir::IrBlock::text(text.to_string()));
                        block_origins
                            .response
                            .push(IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                    }
                }

                if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                    let name_val = func_call
                        .get(keys::NAME)
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();

                    let args = empty_object_if_absent(func_call.get(FIELD_ARGS));

                    let id = gemini_call_id(func_call)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            synth_tool_call_id(tool_call_index, &name_val, tool_id_turn_salt)
                        });
                    tool_call_index += 1;

                    let thought_signature = part
                        .get(FIELD_THOUGHT_SIGNATURE)
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    content.push(crate::codec::ir::IrBlock::ToolUse {
                        id,
                        name: name_val,
                        input: args,
                        cache_control: None,
                        thought_signature,
                    });
                    block_origins
                        .response
                        .push(IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                }

                if let Some(block) = read_gemini_media_part(part) {
                    content.push(block);
                    block_origins
                        .response
                        .push(IrBlockOrigin::at(&RESPONSE_PART_SOURCE));
                }
            }
        }

        attach_gemini_citations_to_text_blocks(candidate, &mut content);

        let stop_reason = candidate
            .get(FIELD_FINISH_REASON)
            .and_then(|r| r.as_str())
            .map(map_gemini_finish_reason);

        let stop_reason = match stop_reason {
            Some(crate::codec::ir::IrStopReason::EndTurn)
                if content
                    .iter()
                    .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. })) =>
            {
                Some(crate::codec::ir::IrStopReason::ToolUse)
            }
            other => other,
        };

        let usage = read_gemini_usage(body)?;

        let model = obj
            .get(FIELD_MODEL_VERSION)
            .or_else(|| obj.get(keys::MODEL))
            .and_then(|m| m.as_str())
            .map(String::from);

        let id = obj
            .get(FIELD_RESPONSE_ID)
            .and_then(|i| i.as_str())
            .map(String::from);

        let logprobs = read_gemini_logprobs(candidate.get(FIELD_LOGPROBS_RESULT));

        Ok(crate::codec::ir::IrResponse {
            block_origins,
            logprobs,
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason,
            usage,
            model,
            id,

            created: read_gemini_create_time(body)
                .as_deref()
                .and_then(gemini_rfc3339_to_epoch),
            system_fingerprint: None,
            stop_sequence: None,
            safety: super::read_safety_ratings(body, candidate),

            request_echo: None,
            stop_detail: None,
            ..Default::default()
        })
    }

    /// IR-18: a streamed `thoughtSignature` on the Gemini wire is Gemini-minted.
    fn stream_signature_origin(
        &self,
        _state: &crate::codec::ir::StreamDecodeState,
    ) -> Option<crate::codec::ir::IrSignatureOrigin> {
        Some(crate::codec::ir::IrSignatureOrigin::Gemini)
    }

    /// A chunk bills or fails only via a top-level `error` object, a `promptFeedback` (prompt-blocked
    /// envelope), or a `finishReason` string on `candidates[0]`; usage is read off those chunks alone.
    fn same_proto_reads(&self, _event_type: &str, payload: &[u8]) -> bool {
        use crate::codec::json_splice::{any_member, first_element};
        any_member(payload, |m, v| {
            (m.is(keys::ERROR_WORD) && v.starts_with(b"{"))
                || m.is(FIELD_PROMPT_FEEDBACK)
                || (m.is(FIELD_CANDIDATES)
                    && first_element(v).is_some_and(|candidate| {
                        any_member(candidate, |m, v| {
                            m.is(FIELD_FINISH_REASON) && v.starts_with(b"\"")
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
}

/// Close every block still open, in order: a thinking block first (a thinking-only stream never
/// opens text, so a terminal path is its only closer), the text block at its claimed index (not a
/// hardcoded 0), then tools in ascending order (via `open_tools`).
fn close_open_blocks(
    state: &mut crate::codec::ir::StreamDecodeState,
    out: &mut Vec<IrStreamEvent>,
) {
    if state.thinking_block_open {
        state.thinking_block_open = false;
        out.push(IrStreamEvent::BlockStop { index: 0 });
    }
    if state.text_block_open {
        state.text_block_open = false;
        let ti = state.text_index.take().unwrap_or(0);
        out.push(IrStreamEvent::BlockStop { index: ti });
    }
    for oai_idx in std::mem::take(&mut state.open_tools) {
        out.push(IrStreamEvent::BlockStop { index: oai_idx });
    }
}

#[cfg(test)]
#[path = "tests/unreadable_count_refusal_tests.rs"]
mod unreadable_count_refusal_tests;

#[cfg(test)]
#[path = "tests/reader_native_origin_tests.rs"]
mod native_origin_tests;
