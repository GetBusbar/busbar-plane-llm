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
    }),
    stream_carried: Some(crate::codec::drops::Carried {
        map: super::map::STREAM_PATHS,
        code: super::STREAM_CODE,
        drops: super::STREAM_DROPS,
        defaults: &[],
    }),
    stream_keyed_by_event: false,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for AnthropicReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;
        // The whole usage table, as the buffered read takes it: a truncated turn that ran web
        // searches ledgers them exactly as its complete twin does. An unreadable billed count is
        // refused on its own and bills nothing; the readable counts recover.
        Some(
            crate::codec::usage_count::read_usage(COUNT_LABEL, Some(&v), USAGE)
                .ok()?
                .to_token_usage(),
        )
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse the error body once and pull both fields from the one tree.
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

        // Anthropic signals context-length via the error message (no distinct code); surface the
        // canonical code so the breaker pipeline maps it to ContextLength.
        //
        // The message scan is gated on 400/413, the only statuses where an oversized-prompt body is
        // authoritative (as in the Cohere `body_signals_context_length` gate). Ungated, a 401/403
        // ("invalid token") or 429 ("rate limit on tokens") would become a non-penalizing
        // ContextLength fail-over and the breaker would never record the auth or rate-limit fault.
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
                Some(busbar_contract::protocol::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
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
    /// The reader's one piece of stream state (the suppressed-block indices in `open_tools`) shapes
    /// only the events a FOREIGN writer re-emits, so skipping every other event cannot desync it.
    /// A frame with no `event:` line reads as `""` and is skipped.
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

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        // Count of `role:"system"` entries folded out of `messages` below, so `IrFacts::shape()`
        // reports the raw `messages` array length.
        let mut system_turns_folded: usize = 0;
        // Where each folded entry stood, for the hook view (`IrRequest::system_folds`).
        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        // Handle system field (string or array)
        if let Some(system_val) = obj.get(keys::SYSTEM) {
            if system_val.is_string() {
                let text = system_val.as_str().unwrap_or("").to_string();
                system_blocks.push(crate::codec::ir::IrBlock::Text {
                    text,
                    cache_control: None,
                    citations: Vec::new(),
                    refusal: false,
                });
            } else if let Some(arr) = system_val.as_array() {
                system_blocks.extend(read_blocks(arr)?);
            }
        }

        // Messages. There is no `system` role inside `messages`, but a body can present one: promote
        // it into `system_blocks` (appended in order after the top-level `system`) so the writer
        // never emits `role:"system"`, which upstream rejects with a 400.
        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        // Positions (post system-filter, matching `write_request`'s indexing) of any raw content
        // block the IR does not hold whole, parked so an Anthropic-to-Anthropic hop through the IR
        // splices the original back. A block the IR does not read at all has nothing standing in.
        let mut unmodeled_blocks: Vec<serde_json::Value> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            // A present but wrong-typed `messages` is a structural violation: reject with a 400
            // rather than coerce to an empty conversation. An absent `messages` stays lenient.
            let messages_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;
            for msg_val in messages_arr {
                let msg = read_message(msg_val)?;
                if msg.role == crate::codec::ir::IrRole::System {
                    system_turns_folded += 1;
                    let blocks_before = system_blocks.len();
                    system_blocks.extend(msg.content);
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
                }
            }
        }
        if !unmodeled_blocks.is_empty() {
            extra.insert(
                ANTHROPIC_UNMODELED_BLOCKS_SENTINEL.to_string(),
                serde_json::Value::Array(unmodeled_blocks),
            );
        }

        // Handle tools array
        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        // A server tool whose kind the IR models (web search, web fetch, code execution) crosses
        // in the typed slot, not as a raw hosted `IrTool`.
        let mut hosted_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_val) = obj.get(keys::TOOLS) {
            // A present `tools` that is not an array is malformed: reject rather than coerce to
            // empty, which would silently strip the caller's tools.
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                match read_hosted_tool(tool_val) {
                    Some(hosted) => hosted_tools.push(hosted),
                    None => tools.push(read_tool(tool_val)?),
                }
            }
        }

        // Extract scalar fields and extra
        // Checked `u32::try_from`: `as u32` would truncate a `max_tokens`/`top_k` above `u32::MAX`
        // (4294967297 to 1). An out-of-range value drops to `None` and the upstream applies its default.
        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            // `max_tokens: 0` is absent, as in every other reader: a zero cap would force an
            // invalid body on egress.
            .filter(|&v| v > 0);
        // temperature / top_p / top_k / stop_sequences are mapping-file rows, read below.
        // `tool_choice` is normalized into the IR union so forced or targeted use survives.
        let tool_choice = read_anthropic_tool_choice(obj.get(keys::TOOL_CHOICE));
        // `disable_parallel_tool_use` rides inside tool_choice; it is inverted ("parallel allowed?")
        // to carry to OpenAI's `parallel_tool_calls`.
        let parallel_tool_calls = obj
            .get(keys::TOOL_CHOICE)
            .and_then(|tc| tc.get(super::DISABLE_PARALLEL_TOOL_USE))
            .and_then(|v| v.as_bool())
            .map(|disabled| !disabled);
        // `metadata.user_id` is Anthropic's spelling of OpenAI's `user`; promote it. The `metadata`
        // object still rides `extra`, keeping same-protocol fidelity byte-exact.
        let user = obj
            .get(keys::METADATA)
            .and_then(|m| m.get(super::USER_ID))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // The request-level `thinking` ask, plus `output_config.effort`:
        //   * `{type:"enabled", budget_tokens:N}` is `Budget(N)`;
        //   * `{type:"adaptive"}` is `Effort(word)` when an effort word is given, else `Dynamic`
        //     ("the model decides", Gemini's `thinkingBudget:-1`);
        //   * no `thinking` but an effort word is `Effort(word)`;
        //   * `{type:"disabled"}` is `Off`, which a reasoning-by-default foreign model must hear
        //     (Chat/Responses `"none"`, Gemini `thinkingBudget:0`) rather than read as "never said";
        //   * a malformed `thinking` is no ask.
        // Everything stays in `extra` too except a promoted budget-form `thinking` (removed below).
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
        // Native structured outputs: `output_config.format` or the deprecated top-level
        // `output_format` of the same shape.
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

        // Unmodeled top-level keys go to `extra`; the modelled keys are the mapping file's rows.
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);
        // A promoted thinking ask must not also ride extra, or the writer double-emits it.
        if reasoning_is_budget {
            extra.remove(keys::THINKING);
        }
        // The deprecated spelling is promoted and re-emitted in the GA spelling; leaving it in
        // `extra` would put both on a same-protocol hop.
        if response_format.is_some() && output_format_legacy.is_some() {
            extra.remove(keys::OUTPUT_FORMAT);
        }

        // No ingress sentinel scrub is needed: `redacted` is a typed flag only the Anthropic and
        // Bedrock readers set on a genuine redacted block, so a client cannot forge one.

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
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
            hosted_tools,
            ..Default::default()
        };
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        Ok(ir)
    }

    fn read_response_event(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Option<IrStreamEvent> {
        match event_type {
            EVT_MESSAGE_START => {
                let msg = data.get(keys::MESSAGE)?;
                let role_str = msg.get(keys::ROLE).and_then(|r| r.as_str())?;
                let role = match role_str {
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                    _ => return None,
                };
                // BILLED COUNTS: absent is zero, UNREADABLE REFUSES — the stream ends in an
                // error instead of ledgering "no work happened" for a count the provider sent.
                let usage = match data
                    .get(keys::MESSAGE)
                    .and_then(|m| m.get(keys::USAGE))
                    .map(|u| read_anthropic_usage(Some(u)))
                    .transpose()
                {
                    Ok(usage) => usage,
                    Err(refusal) => return Some(IrStreamEvent::Error(refusal)),
                };
                // Capture the stream's native `message.id`/`message.model` so a same-protocol hop
                // re-emits the `message_start.message` an SDK expects. `message_start` has no
                // `created`, so it stays None.
                let id = msg.get(keys::ID).and_then(|i| i.as_str()).map(String::from);
                // Empty `model` maps to `None`: the writer emits `model: ""` as its fallback, so
                // reading it back as `None` keeps the round-trip idempotent.
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
                    // A `citations_delta` carries one `citation`; `read_citation` fills the neutral
                    // fields and the byte-exact `raw`, carried as `IrDelta::CitationsDelta`.
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
                // `delta.stop_sequence`: the matched stop string, present only when one fired.
                let stop_sequence = delta
                    .get(keys::STOP_SEQUENCE)
                    .and_then(|s| s.as_str())
                    .map(String::from);
                // `usage` is optional here: do not `?` it. `message_delta` is the terminal event
                // carrying `stop_reason`, and dropping it would hide whether generation completed. An
                // Anthropic-compatible backend may omit `usage`, so counters zero-default as in the
                // `message_start` reader. An absent count zero-defaults; a present but unreadable one
                // refuses (the stream ends in an error) rather than bill zero.
                let usage_val = data.get(keys::USAGE);
                let usage = match read_anthropic_usage(usage_val) {
                    Ok(usage) => usage,
                    Err(refusal) => return Some(IrStreamEvent::Error(refusal)),
                };
                // The context-window refinement of a length stop and a refusal's `stop_details`
                // category, carried beside the coarse reason.
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
                let err_val = data.get(keys::ERROR_WORD)?;
                // Carry the upstream error `type` as-is, `None` when omitted. Do not default it to
                // `Some("")`: the writer would emit `"type": ""` where native carries a real type
                // or `null`.
                let type_token = err_val.get(keys::TYPE).and_then(|t| t.as_str());
                let provider_signal = type_token.map(String::from);
                // Derive the breaker class from the error `type`, as the HTTP classifier does: a
                // mid-stream `overloaded_error`/`rate_limit_error`/`api_error` is a transient
                // upstream fault, and a fixed ClientError would hide it from the breaker.
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
        host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        // Suppressed blocks: a block type the IR does not model on a stream (`server_tool_use`,
        // `web_search_tool_result`, `mcp_tool_use`, a future type) produces no `BlockStart`, so its
        // deltas and stop would reach a foreign client for a block it never saw open. Remember
        // each suppressed index and drop everything at it. The set lives in the per-stream decode
        // state (`open_tools`), the reader's only per-stream memory.
        if event_type == EVT_CONTENT_BLOCK_START {
            let block_type = data
                .get(super::CONTENT_BLOCK)
                .and_then(|b| b.get(keys::TYPE))
                .and_then(|t| t.as_str());
            if let (Some(index), Some(t)) = (read_clamped_block_index(data), block_type) {
                if !is_streamed_anthropic_block_type(t) {
                    state.open_tools.insert(index);
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
        // A streamed `redacted_thinking` block carries its opaque `data` inline on the
        // `content_block_start` (no deltas), so emit the pair the IR models for it, a `Thinking`
        // BlockStart plus a `RedactedReasoningDelta` carrying the bytes, from this one event.
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
        // Anthropic events are otherwise already block-structured (1:1): wrap the singular.
        match self.read_response_event(host, event_type, data) {
            Some(ev) => vec![ev],
            None => vec![],
        }
    }

    fn read_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        // Parse role (should be "assistant" for responses)
        let role_str = obj.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");
        let role = match role_str {
            keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
            _ => return Err(ir_parse_error()),
        };

        // Parse content blocks
        let content_val = obj.get(keys::CONTENT).ok_or_else(ir_parse_error)?;
        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        if let Some(arr) = content_val.as_array() {
            // A block kind the reader does not model is dropped, never answered as an empty text
            // block; a translate attempt names it (`RESPONSE_BLOCKS`).
            for block_val in arr
                .iter()
                .filter(|b| super::RESPONSE_BLOCKS.iter().all(|g| g.models(b)))
            {
                content.push(read_block(block_val)?);
            }
        }

        // Parse stop_reason (optional)
        // A lane WITH native structured outputs (`LaneCaps::native_structured_output`) asks through
        // `output_config.format`, so its answer is ordinary text; a lane without it never received
        // the directive (1.5.5's drop), so no answer block is rewritten here.
        let stop_reason = obj
            .get(super::STOP_REASON)
            .and_then(|r| r.as_str())
            .map(read_anthropic_stop_reason);

        // `usage` is optional: an Anthropic-compatible backend may omit it, and requiring it would
        // turn a valid 200 into a 400 (the streaming readers zero-default too). An absent counter
        // is zero; a present but unreadable one refuses instead of ledgering zero.
        let usage_val = obj.get(keys::USAGE);
        // The 5m/1h cache-creation tier split rides the same table as the totals (see `USAGE`);
        // the kernel counts the tiers separately, so collapsing them breaks per-line reconciliation.
        let usage = read_anthropic_usage(usage_val)?;

        // An empty `model` is absent: the writer emits `model: ""` as its fallback, and reading it
        // back as `None` keeps the round-trip idempotent.
        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        // Capture the native response identity (`id`, `stop_sequence`) so a same-protocol hop
        // preserves it; `type` is a constant the writer re-emits.
        let id = obj.get(keys::ID).and_then(|i| i.as_str()).map(String::from);
        // `Message` has no `created`, and `system_fingerprint` is an OpenAI concept; both stay None
        // so a same-protocol round-trip invents neither.
        let stop_sequence = obj
            .get(keys::STOP_SEQUENCE)
            .and_then(|s| s.as_str())
            .map(String::from);

        Ok(crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role,
            content,
            stop_reason,
            usage,
            model,
            id,
            created: None,
            system_fingerprint: None,
            stop_sequence,

            request_echo: None,
            // The buffered twin of the `message_delta` read above.
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
