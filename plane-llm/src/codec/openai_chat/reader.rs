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
        // Every count is read through `USAGE`: an unreadable one is refused on its own and bills
        // nothing, the readable ones recover. A truncated body reads the same cache tiers.
        Some(read_openai_usage(Some(&v), None).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse the error body once and derive both fields from the one tree.
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

        // Message-aware, as in openai_responses / anthropic. Some upstreams send a null `code` and
        // carry the overflow only in prose (`This model's maximum context length is 8192 tokens…`);
        // unscanned it would normalize to a generic client error and penalize the lane instead of
        // failing over. When no canonical code was parsed, scan the lowercased message.
        //
        // The scan must be precise: a quota body like `You have reached the maximum number of
        // tokens allowed per day` pairs a stray `maximum` with a stray `token` and would falsely
        // fail over unpenalized. Require a co-located phrase: a self-contained canonical one, or
        // `exceeds` paired with `context`/`token limit`. Gate to the statuses OpenAI uses for an
        // oversized request (400, 413) so a 429/5xx mentioning tokens is never reclassified.
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
        // Both bearer-envelope dialects classify alike (shared dialect module).
        crate::codec::dialect_classify::bearer(status.as_u16(), body)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        // Count of `system`/`developer` entries folded out of `messages` below, so
        // `IrFacts::shape()` reports the raw wire array length.
        let mut system_turns_folded: usize = 0;
        // Which role the folded system entries were written in: `(saw system, saw developer)`.
        let mut system_roles_seen = (false, false);
        // Where each folded entry stood, for the hook view (`IrRequest::system_folds`).
        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();

        // The output-token cap. `max_tokens` is legacy; `max_completion_tokens` is current and
        // mandatory for reasoning models (o1/o3), which reject `max_tokens`. Fall back to it so a
        // request carrying only the modern field still populates the IR `max_tokens` (else it stays
        // in `extra`, cleared at the cross-protocol seam, silently dropping the limit).
        // Narrowed with `u32::try_from`, not `as u32`, which would wrap a value above `u32::MAX`;
        // `as_u64` rejects negatives and non-integers, and the final `> 0` filter rejects a zero cap.
        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .or_else(|| obj.get(MAX_COMPLETION_TOKENS))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0);
        // Remember whether the cap arrived as `max_completion_tokens` (and not `max_tokens`) so a
        // same-protocol passthrough to an o1/o3 model re-emits the key those models require. Only
        // when `max_tokens` is absent; if both are present `max_tokens` is correct. The sentinel
        // rides `extra`, cleared on the cross-protocol seam.
        let max_completion_tokens_was_source =
            !obj.contains_key(keys::MAX_TOKENS) && obj.contains_key(MAX_COMPLETION_TOKENS);
        // The sampling controls (temperature, top_p, the penalties, seed, n, stop) are mapping-file
        // rows, read below. `response_format` is stored verbatim for the writer to re-emit.
        let response_format = obj
            .get(keys::RESPONSE_FORMAT)
            .and_then(read_openai_response_format);
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Handle messages array
        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        // Per-message participant names, collected during the loop and parked in `extra` afterwards
        // (see `MESSAGE_NAMES_SENTINEL`).
        let mut message_names = serde_json::Map::new();
        // Per-message provider-specific fields with no cross-protocol analog (an `audio`
        // reference, the legacy `function_call`, future keys), parked verbatim by IR-message index
        // under `MESSAGE_EXTRAS_SENTINEL` for a same-protocol re-serialize; they drop across
        // protocols (named in the dropped-keys warn).
        let mut message_extras = serde_json::Map::new();
        // Legacy function calling: every assistant `function_call` gets a synthesized `call_…` id,
        // and a later `role:"function"` result pairs with the most recent unmatched call of the same
        // name (the legacy wire correlates by name alone).
        let mut legacy_calls: Vec<(String, String, bool)> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            let msgs_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;

            for msg_val in msgs_arr.iter() {
                let role_str = msg_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let content_val = msg_val.get(keys::CONTENT);

                // `content` is legally a string, an array of parts, or absent/`null` (a turn with only
                // `tool_calls`). Any other present type is a violation the lenient projection below
                // would coerce to an empty turn: reject with the same 400 as the `messages` array.
                if let Some(cv) = content_val {
                    if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                        return Err(ir_parse_error());
                    }
                }

                let role = match role_str {
                    // o1/o3 replace "system" with "developer" (as the Responses reader treats them);
                    // both map to the IR System role and the promotion path below.
                    keys::DEVELOPER | keys::SYSTEM => crate::codec::ir::IrRole::System,
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                    // The legacy function-result turn is a tool result correlated by name.
                    keys::TOOL | keys::FUNCTION => crate::codec::ir::IrRole::Tool,
                    _ => return Err(ir_parse_error()),
                };

                // Promote every system-role message to the top-level system field, wherever it
                // stands: OpenAI permits system turns anywhere, but Anthropic and the IR contract
                // need them top-level (a System message in the array would 400 there). A System
                // IrMessage is never pushed; its content accumulates into system_blocks.
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
                        } else if let Some(arr) = content.as_array() {
                            for block_val in arr {
                                system_blocks.extend(read_openai_part(block_val)?);
                            }
                        }
                    }
                    // A degenerate system message (content omitted, null, or an empty array) must not
                    // vanish: emit an empty Text block. A turn whose parts were all unmodeled kinds
                    // is not empty: those parts are dropped (named at a translate seam).
                    let had_parts = content_val
                        .and_then(|c| c.as_array())
                        .is_some_and(|a| !a.is_empty());
                    if system_blocks.len() == blocks_before && !had_parts {
                        system_blocks.push(crate::codec::ir::IrBlock::text(String::new()));
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

                    // A Tool-role message's `content` is the tool result, captured below as the
                    // `ToolResult` block's inner content. Also pushing it as a standalone Text block
                    // would duplicate the output (and surface a spurious `{"role":"tool"}` message
                    // on write), so the ToolResult path owns it.
                    if role != crate::codec::ir::IrRole::Tool {
                        if let Some(cv) = content_val {
                            if let Some(text) = cv.as_str() {
                                msg_content.push(crate::codec::ir::IrBlock::text(text.to_string()));
                            } else if let Some(arr) = cv.as_array() {
                                for block_val in arr {
                                    msg_content.extend(read_openai_part(block_val)?);
                                }
                            }
                        }
                    }

                    // Handle tool_calls for assistant messages
                    if role == crate::codec::ir::IrRole::Assistant {
                        if let Some(tool_calls) = msg_val.get(keys::TOOL_CALLS) {
                            if let Some(tc_arr) = tool_calls.as_array() {
                                for tc_val in tc_arr {
                                    // A replayed custom-tool call (free-text input) has no IR
                                    // tool-call form but is a valid native message the reader
                                    // must not refuse: it is dropped, observably.
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
                                    // A tool call must carry a non-empty string `id`, the key that
                                    // pairs the eventual result; reject rather than invent one.
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
                                }
                            }
                        }
                    }

                    // The legacy `function_call{name, arguments}` on an assistant turn becomes a
                    // ToolUse with a synthesized id, so it survives replay to another dialect. The raw
                    // member also stays in the extras stash, which an OpenAI-origin re-serialize
                    // writes back instead of `tool_calls`.
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
                        }
                    }

                    // Handle tool results
                    if role == crate::codec::ir::IrRole::Tool {
                        let tool_call_id = if role_str == keys::FUNCTION {
                            // Legacy result: pair by name with the latest unmatched legacy call; a
                            // result with no such call keeps a synthesized id of its own.
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
                        // Tool-message `content` may be a string or an array of parts, both legal.
                        // A string is used verbatim; an array is parsed via `read_openai_block` and
                        // its text parts concatenated (non-text parts contribute nothing), so
                        // array-form tool output is not collapsed to an empty string.
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
                    }

                    // A message-level `refusal` string (echoed into replayed history) carries the
                    // content of a `refusal` part. Map it to a Text block flagged as a refusal, so it
                    // survives a cross-protocol hop as plain assistant text; a same-protocol
                    // re-serialize writes it back as a `refusal` part.
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
                    }

                    // The per-message participant `name`, parked under `MESSAGE_NAMES_SENTINEL` by IR
                    // message index: it survives a same-protocol re-serialize and is named in the
                    // cross-protocol dropped-keys warn.
                    if let Some(name) = msg_val
                        .get(keys::NAME)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty() && role_str != keys::FUNCTION)
                    {
                        message_names.insert(messages.len().to_string(), serde_json::json!(name));
                    }

                    // Stash any message-level field this reader does not model, by IR-message index, so
                    // a provider-specific construct survives a same-protocol re-serialize. Keys already
                    // in typed IR (`content`/`tool_calls`/`name`/`refusal`) and the structural
                    // `role`/`tool_call_id` are excluded so the writer never double-emits.
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
                        // A legacy `role:"function"` turn is remembered (with its `name`) so a
                        // re-serialize writes it back in the legacy shape, not as a `tool` message.
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
                }
            }
        }

        // Handle tools array
        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        let mut custom_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_val) = obj.get(keys::TOOLS) {
            // A present `tools` that is not an array is malformed: reject rather than coerce to
            // empty, which would silently strip the caller's tools.
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                // A `custom` tool crosses in the typed hosted-tool slot so a Responses lane receives
                // it; one with a member the IR cannot hold stays the raw same-protocol tool.
                if let Some(custom) = super::slots::read_custom_tool(tool_val) {
                    custom_tools.push(custom);
                    continue;
                }
                tools.push(read_openai_tool(tool_val)?);
            }
        } else if let Some(functions) = obj.get(FUNCTIONS).and_then(|f| f.as_array()) {
            // The legacy `functions` array: each entry is the body of a modern function tool. It
            // also stays in `extra`, which a re-serialize writes back instead of `tools`.
            for f in functions {
                tools.push(read_openai_tool(&serde_json::json!({
                    (keys::TYPE): TOOL_TYPE_FUNCTION,
                    (keys::FUNCTION): f,
                }))?);
            }
        }

        // Unmodeled top-level keys flow through `extra` verbatim (logit_bias, …) so a same-protocol
        // passthrough reaches the upstream unchanged. Keys the IR models as first-class, including
        // the promoted penalties, seed, n and response_format, are excluded or the writer would
        // double-emit them. The modelled keys are the mapping file's top-level rows.
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        // Stamp the sentinel when the cap arrived as `max_completion_tokens`, only if it produced a
        // usable value. Same-protocol only: `extra` is cleared across protocols.
        if max_completion_tokens_was_source && max_tokens.is_some() {
            extra.insert(
                MAX_COMPLETION_TOKENS_SENTINEL.to_string(),
                serde_json::Value::Bool(true),
            );
        }
        // Park the participant names only when a message carried one, so an ordinary request's
        // `extra` (and its dropped-keys warn) is untouched.
        if !message_names.is_empty() {
            extra.insert(
                MESSAGE_NAMES_SENTINEL.to_string(),
                serde_json::Value::Object(message_names),
            );
        }
        // Park the provider-specific extras likewise, same-protocol only.
        if !message_extras.is_empty() {
            extra.insert(
                MESSAGE_EXTRAS_SENTINEL.to_string(),
                serde_json::Value::Object(message_extras),
            );
        }

        // `tool_choice` is a first-class IR control so a forced or targeted directive survives the
        // seam. The legacy `function_call` directive is its pre-`tools` spelling, read when
        // `tool_choice` is absent.
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

        // Cross-protocol carries with an Anthropic analog: `user` <-> `metadata.user_id`,
        // `parallel_tool_calls` <-> `!tool_choice.disable_parallel_tool_use`.
        let user = obj
            .get(keys::USER)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parallel_tool_calls = obj.get(keys::PARALLEL_TOOL_CALLS).and_then(|v| v.as_bool());

        // The reasoning ask: a top-level `reasoning_effort` word, promoted so it carries to
        // Anthropic/Gemini thinking budgets via the effort table.
        let reasoning_effort_raw = obj.get(REASONING_EFFORT).and_then(|v| v.as_str());
        // `xhigh` (gpt-5's step above `high`) is the IR's `XHigh`, and `none` is reasoning switched
        // off (`IrReasoningAsk::Off`, not "said nothing"), so a cross-protocol lane gets the top
        // effort or disabled thinking. (`max` is not a Chat word.) The raw word still rides `extra`
        // so a re-serialize writes it back verbatim.
        let reasoning = reasoning_effort_raw.and_then(|raw| match raw {
            keys::NONE_WORD => Some(crate::codec::ir::IrReasoningAsk::Off),
            "xhigh" => Some(crate::codec::ir::IrReasoningAsk::Effort(
                crate::codec::ir::IrReasoningEffort::XHigh,
            )),
            other => crate::codec::ir::IrReasoningEffort::parse(other)
                .map(crate::codec::ir::IrReasoningAsk::Effort),
        });
        // `reasoning_effort` is a modeled key, excluded from the generic `extra` sweep, so a value
        // `IrReasoningEffort::parse` does not know would be lost even OpenAI to OpenAI. The
        // `OnceLock<HashSet>` behind `modeled_request_keys()` cannot vary per request, so
        // re-inserting here is the only rescue.
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

        // Logprobs ask, first-class so it reaches Gemini as `generationConfig.responseLogprobs`.
        let logprobs = obj.get(keys::LOGPROBS).and_then(|v| v.as_bool());
        let top_logprobs = obj
            .get(keys::TOP_LOGPROBS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let mut ir = crate::codec::ir::IrRequest {
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
            // `developer` only when every folded entry was one, `system` likewise; mixed says nothing.
            system_role: match system_roles_seen {
                (true, false) => Some(crate::codec::ir::IrSystemRole::System),
                (false, true) => Some(crate::codec::ir::IrSystemRole::Developer),
                _ => None,
            },
            ..Default::default()
        };
        // The flat request fields (`map.gen.rs`), then the allowed-tools subset of `tool_choice`.
        crate::codec::carry::read_fields(super::map::REQUEST, obj, &mut ir);
        super::slots::read_tool_choice_slots(obj, &mut ir);
        Ok(ir)
    }

    /// OpenAI's flat stream → IR block-structured events. One chat.completion.chunk
    /// may carry role + content + finish at once → up to several IR events. State synthesizes the
    /// block boundaries OpenAI doesn't have.
    fn read_response_events(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();

        // [DONE] sentinel (or any non-object) carries no IR events.
        if data.as_str() == Some(crate::codec::dialect::SSE_DONE_SENTINEL) {
            return out;
        }

        // 0. Inline error envelope. An upstream that fails after the 200 headers (OpenAI, Azure,
        //    vLLM, OpenRouter) delivers `{"error":{"message","type","code"}}` as a data chunk with no
        //    `choices`. Every guard below keys off `choices`, so unhandled the stream ends as a clean
        //    success: no `IrStreamEvent::Error`, no breaker fault, the partial completion billed as
        //    finished, and no hint to a cross-protocol client that it was truncated. The engine
        //    converts only HTTP-status errors. As in the gemini/bedrock/anthropic/openai_responses
        //    readers, map the envelope's vocabulary to a `StatusClass` and push one
        //    `IrStreamEvent::Error`.
        //
        //    Handled before MessageStart so an error-only chunk never emits a stray start frame.
        if let Some(error_obj) = data.get(keys::ERROR_WORD).and_then(|e| e.as_object()) {
            let error_type = error_obj.get(keys::TYPE).and_then(|t| t.as_str());
            let code = error_obj.get(keys::CODE).and_then(|c| c.as_str());
            // The most specific upstream token is the provider signal (`code`, else `type`, else
            // `message`); `None` only when the envelope carries none.
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

        // 1. MessageStart once, on the first chunk regardless of delta.role, capturing its `id`,
        //    `created` and `model` for a same-protocol stream to re-emit.
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
        // A client can request n>1. This reader collapses to choices[0], all a cross-protocol hop
        // can carry; a same-protocol relay re-emits upstream bytes and keeps all N (this reader
        // still runs there as a usage side-channel), so the warning is scoped to the cross-protocol
        // case. Once per stream. The engine also rejects n>1 up front on cross-protocol routes.
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

        // 2. Reasoning (chain-of-thought) becomes a Thinking block. Reasoning before any answer
        //    block opens at index 0 and `reasoning_seen` reserves that slot (the claim counter
        //    starts at 1).
        //
        //    Reasoning after the answer phase began (interleaved, from compatible backends) closes
        //    an open text block and opens a new Thinking block at the next free index from the
        //    monotone counter, so no opened index shifts. Its index is recorded under
        //    `LATE_THINKING_KEY` for later deltas and its stop.
        if let Some(reasoning) = delta
            .and_then(|d| d.get(REASONING_CONTENT).or_else(|| d.get(keys::REASONING)))
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
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

        // 3. Text content: close any open thinking block, then open the text block and a TextDelta.
        //    Text claims its index from the monotone counter (`claim_ir_index`), which reserves 0
        //    for a thinking block via `reasoning_seen`.
        // `delta.refusal` carries a refusal in the same incremental shape as `delta.content`, mutually
        // exclusive on a chunk. It routes through the same text block (Anthropic/Bedrock/Gemini have
        // no refusal part); reading only `content` would give an empty end_turn 200 the client could
        // not tell from silence. `refusal_seen` promotes the stop reason on the terminal frame.
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
        // The refusal rides its own text block, opened with `refusal: true`, so a writer with a
        // refusal slot carries it exactly. A non-empty refusal wins over an empty `content`.
        let (text_delta, is_refusal) = match (content_delta, refusal_delta) {
            (Some(c), _) if !c.is_empty() => (Some(c), false),
            (_, Some(r)) => (Some(r), true),
            (c, None) => (c, false),
        };
        // A non-empty delta of the OTHER kind than the open text block closes it, so a refusal and
        // ordinary content never share one block.
        if state.text_block_open
            && text_delta.is_some_and(|t| !t.is_empty())
            && is_refusal != open_text_is_refusal(state)
        {
            close_text_block(state, &mut out);
        }
        if let Some(content) = text_delta
            // An EMPTY delta after the text block closed carries nothing and must not open a new,
            // empty text block.
            .filter(|c| !(state.text_block_closed && c.is_empty()))
        {
            close_thinking_block(state, &mut out);
            // Text resuming after its block closed (by a `tool_calls` chunk or late reasoning) opens
            // a new block at the next free index: no second `BlockStart` at the closed one, and
            // no silent drop of the answer.
            if state.text_block_closed {
                state.text_block_closed = false;
                state.text_index = None;
            }
            let ti = match state.text_index {
                Some(i) => i,
                None => {
                    // Claim the next free IR index by order of first appearance: a tool that arrived
                    // earlier already claimed its slot from the same counter, whatever the upstream
                    // tool_calls[].index values.
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

        // 3b. Per-chunk logprobs ride the choice (`choices[].logprobs.content[]`), not the delta.
        //     Carry them as a LogprobsDelta on the text block's index so a foreign stream can
        //     re-emit them natively; a logprobs-only chunk still opens the text block.
        // Gate on `!text_block_closed` as in step 3: a logprobs chunk after `tool_calls` closed the
        // text block must not reopen it (duplicate `BlockStart`); drop it rather than un-balance.
        let lp_entries = if state.text_block_closed {
            Vec::new()
        } else {
            crate::codec::logprob_wire::read_token_logprobs(
                choice0.and_then(|c| c.get(keys::LOGPROBS)),
            )
        };
        if !lp_entries.is_empty() {
            if !state.text_block_open {
                // Close any open thinking block first (a reasoning backend can stream logprobs
                // meanwhile), or two blocks stay open and the IR stream unbalances, as in steps 3 and 4.
                close_thinking_block(state, &mut out);
                state.text_block_open = true;
                let ti = state.text_index.unwrap_or_else(|| {
                    let i = state.claim_ir_index();
                    state.text_index = Some(i);
                    i
                });
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

        // 3c. Grounding sources stream as `choices[].delta.annotations` (`url_citation` entries, as
        //     in buffered `message.annotations`). They ride a CitationsDelta on the text block,
        //     opening it if needed as a logprobs-only chunk does. Offsets are not carried (see
        //     `read_url_annotations`).
        let citations = delta
            .and_then(|d| d.get(keys::ANNOTATIONS))
            .map(super::super::url_citation_wire::read_url_annotations)
            .unwrap_or_default();
        if !citations.is_empty() {
            if state.text_block_closed && !state.text_block_open {
                // The annotated text is already closed (a tool call intervened) and a delta at a
                // closed index would un-balance the stream; say so rather than detach the sources.
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

        // 4. Tool calls claim an IR block index from the monotone counter by order of first
        //    appearance, like text. `oai_idx` (upstream `tool_calls[].index`) is a join key pairing
        //    a tool's id/name chunk with its later `arguments` chunks, not an index space: a backend
        //    streaming `tool_calls[{index:1}]` with no index 0 (vLLM / Azure / OpenRouter) would
        //    collide with or leave a gap before the text block. BlockStart on first sight
        //    (id+name present), InputJsonDelta for arguments.
        //    The legacy `delta.function_call{name, arguments}` streams like one tool call with no id:
        //    it rides the same path under the reserved `LEGACY_FUNCTION_CALL_KEY` with a synthesized
        //    `call_…` id, so it does not vanish while `finish_reason: "function_call"` reports a
        //    tool use with no block.
        let tool_items: Vec<(usize, Option<&str>, Option<&serde_json::Value>)> = match delta
            .and_then(|d| d.get(keys::TOOL_CALLS))
            .and_then(|t| t.as_array())
        {
            Some(tcs) => tcs
                .iter()
                .map(|tc| {
                    // Bound the upstream tool-call index (a chunk can carry `u64::MAX`; OpenAI
                    // documents at most 128 parallel calls) before it keys `open_tools` /
                    // `tool_ir_index`. `claim_ir_index()` bounds the IR index, so `oai_idx` never
                    // enters index arithmetic; the clamp only bounds it as a map/set key.
                    let oai_idx = tc
                        .get(keys::INDEX)
                        .and_then(|i| i.as_u64())
                        .map_or(0, |v| v.min(MAX_TOOL_INDEX) as usize);
                    (
                        oai_idx,
                        tc.get(keys::ID).and_then(|i| i.as_str()),
                        tc.get(keys::FUNCTION),
                    )
                })
                .collect(),
            None => delta
                .and_then(|d| d.get(keys::FUNCTION_CALL))
                .filter(|f| f.is_object())
                .map(|f| vec![(LEGACY_FUNCTION_CALL_KEY, None, Some(f))])
                .unwrap_or_default(),
        };
        if !tool_items.is_empty() {
            // A tool call means the answer phase has begun; close any still-open thinking block.
            close_thinking_block(state, &mut out);
            // Also close a still-open text block (preamble then tool), or two content blocks
            // overlap and break the strict bracketing asserted downstream. The close latches the
            // text index (`text_block_closed`), so text resuming after the tool calls (vLLM / Azure /
            // OpenRouter) opens a new block at the next free index.
            close_text_block(state, &mut out);
            for (oai_idx, tc_id, func) in tool_items {
                if let Some(name) = func
                    .and_then(|f| f.get(keys::NAME))
                    .and_then(|n| n.as_str())
                {
                    // Cap distinct open tool calls per stream at OpenAI's parallel-call limit, or a
                    // backend emitting unbounded unique indices grows `open_tools` (and BlockStarts)
                    // without limit, a memory DoS. An index beyond the cap that is not already open
                    // has its BlockStart suppressed; an already-open index is always honored.
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
                        // Record the IR index this BlockStart used; arg deltas and the finish-path
                        // BlockStop replay it, since the counter never revisits a claimed slot.
                        state.tool_ir_index.insert(oai_idx, ir_idx);
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
                    // Route argument deltas only to an index with a BlockStart. `open_tools` and
                    // `tool_ir_index` are inserted together (the over-cap path inserts neither), so a
                    // missing entry means no BlockStart: emit nothing.
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

        // Read top-level `usage` independently of finish_reason. Under `include_usage` the API emits
        // usage in a separate trailing chunk with empty `choices` and no finish_reason (`choice0`
        // is None, so the finish branch never runs); reading it only inside that branch would
        // discard the trailing usage chunk.
        //
        // Critical: under `include_usage` every non-final chunk carries `usage: null`, which
        // `Value::get` returns as `Some(Null)`; a naive `.map(...)` would synthesize zero usage on
        // every content chunk and emit a spurious `MessageDelta` each. Filter to a real object.
        //
        // Billed counts: absent is zero, a present but unreadable count refuses (the stream ends
        // in an error) instead of ledgering zero. The sub-buckets ride the stream's usage chunk
        // too, read through the same table; the serving tier is top-level, beside `usage`.
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

        // 5. finish_reason → close open blocks (text first, then tools ascending), MessageDelta, MessageStop.
        let finish_reason = choice0
            .and_then(|c| c.get(keys::FINISH_REASON))
            .and_then(|r| r.as_str());
        if let Some(fr) = finish_reason {
            // Close in order: thinking (if it never yielded to text), then text, then tools.
            close_thinking_block(state, &mut out);
            if state.text_block_open {
                state.text_block_open = false;
                // `text_block_open` implies `text_index.is_some()` (set together), so no index is fabricated.
                if let Some(ti) = state.text_index {
                    out.push(IrStreamEvent::BlockStop { index: ti });
                }
            }
            // Replay each tool's BlockStop at the IR index its BlockStart used, from
            // `tool_ir_index`; it was claimed once and never recomputed. `open_tools` and
            // `tool_ir_index` are taken here, so `next_ir_index` is the only state carried forward
            // and a post-finish chunk claims a fresh slot.
            let tool_ir_index = std::mem::take(&mut state.tool_ir_index);
            for oai_idx in std::mem::take(&mut state.open_tools) {
                if let Some(&index) = tool_ir_index.get(&oai_idx) {
                    out.push(IrStreamEvent::BlockStop { index });
                }
            }
            let stop_reason = match read_openai_stop_reason(fr) {
                // A refusal reports `finish_reason: "stop"`. Only EndTurn is overridden — a refusal
                // that also hit the length cap keeps MaxTokens.
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
                // A finish chunk without usage still names the serving tier (OAI-03).
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
                // OpenAI has no stop_sequence analog in its stream.
                stop_sequence: None,
                usage,
                stop_detail: None,
            });
            out.push(IrStreamEvent::MessageStop);
        } else if let Some(usage) = chunk_usage {
            // Trailing usage-only chunk (include_usage): no finish_reason and a real top-level
            // `usage` with empty `choices`. Emit a MessageDelta with the late usage so consumers
            // that fold it (Bedrock ingress builds its `metadata` frame from this) see real counts.
            //
            // `choice0.is_none()` guards the genuine shape: a content chunk never reaches this
            // branch even if an intermediary attached usage to it. This reader is ingress-agnostic
            // and emits the faithful IR; `StreamTranslate::translate_event` drops a terminal-class
            // MessageDelta after MessageStop for non-eventstream ingress, where ordering matters.
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
    /// `finish_reason` STRING on `choices[0]` (the finish chunk's usage, zero or real, and its
    /// serving tier); the content chunks between carry none.
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

        // Get choices array
        let choices_val = obj.get(CHOICES).ok_or_else(ir_parse_error)?;
        let choices = choices_val.as_array().ok_or_else(ir_parse_error)?;

        if choices.is_empty() {
            return Err(ir_parse_error());
        }

        // n>1 collapses to choices[0], all a cross-protocol hop can carry; a same-protocol relay
        // re-emits the body and keeps all N (this reader still runs as a usage side-channel). The
        // engine also rejects n>1 up front on cross-protocol routes.
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

        // Parse content (may be null)
        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();

        // Compatible providers (GLM, DeepSeek) emit chain-of-thought in `reasoning_content` (or
        // `reasoning`). Map it to a Thinking block ahead of the answer so it survives translation to
        // protocols that have one; others drop it on write.
        for key in [REASONING_CONTENT, keys::REASONING] {
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
                    break;
                }
            }
        }

        if let Some(content_val) = message_val.get(keys::CONTENT) {
            if let Some(text) = content_val.as_str() {
                if !text.is_empty() {
                    // `annotations` is a sibling of `content` on `message`. See
                    // `read_url_annotations` for why offsets are not carried.
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
                }
            } else if let Some(arr) = content_val.as_array() {
                for block_val in arr {
                    let Some(block) = read_openai_part(block_val)? else {
                        continue;
                    };
                    // An image part in a response message array has no Chat response
                    // representation, so it is dropped observably with a `warn!`.
                    if matches!(block, crate::codec::ir::IrBlock::Image { .. }) {
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
                    }
                }
            }
        }

        // A refusal arrives as `content: null` plus a `refusal` string with `finish_reason:
        // "stop"`, so neither carries the signal; reading only `content` gives an empty response
        // (`content: []` cross-protocol, an index error for an SDK reading `content[0]`). Carry the
        // text as assistant Text and promote the stop reason below, as the Responses reader does.
        let mut saw_refusal = false;
        if let Some(text) = message_val.get(keys::REFUSAL).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                saw_refusal = true;
                // Flagged as a refusal so a dialect with a refusal slot carries it exactly.
                content.push(crate::codec::ir::IrBlock::Text {
                    text: text.to_string(),
                    cache_control: None,
                    citations: Vec::new(),
                    refusal: true,
                });
            }
        }

        // Parse tool_calls
        if let Some(tool_calls_val) = message_val.get(keys::TOOL_CALLS) {
            if let Some(tc_arr) = tool_calls_val.as_array() {
                for (tc_ordinal, tc_val) in tc_arr.iter().enumerate() {
                    // A response tool-call id pairs a later `tool` message, and a blank one is
                    // rejected by a target reader (e.g. Anthropic `tool_use.id`). Rather than fail
                    // an otherwise-good body, synthesize a deterministic `call_…` id when the
                    // backend supplied none.
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
                }
            }
        }

        // The legacy `message.function_call{name, arguments}` (older compatible backends, with
        // `finish_reason: "function_call"`) is a ToolUse minus the id: read it with a synthesized
        // `call_…` id so the call is not lost behind a ToolUse stop with no block. Only when there
        // are no `tool_calls` (the two never coexist).
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
            }
        }

        // Parse finish_reason → stop_reason mapping
        let finish_reason = choice
            .get(keys::FINISH_REASON)
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let mut stop_reason = if finish_reason.is_empty() {
            None
        } else {
            Some(read_openai_stop_reason(finish_reason))
        };
        // A refusal reports `finish_reason: "stop"`, so the signal only survives if it is promoted
        // here. Only EndTurn is overridden — a refusal that also hit the length cap keeps MaxTokens.
        if saw_refusal && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn) {
            stop_reason = Some(crate::codec::ir::IrStopReason::Refusal);
        }

        // An absent `usage` falls back to zero counts rather than erroring: it is an upstream quirk
        // (a mock/proxy backend omitting it on a valid 200), and a `ClientError` would make the
        // engine discard a valid body and emit a spurious 500. Gemini and Cohere tolerate it too.
        let usage_val = obj.get(keys::USAGE);
        //
        // Absent is zero, a present but unreadable count refuses. See `USAGE` for the cache
        // normalization and sub-buckets; the serving tier is top-level, beside `usage`.
        let usage = read_openai_usage(usage_val, obj.get(keys::SERVICE_TIER))?;

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        // Capture the response identity (`id`, `created`, `system_fingerprint`) for same-protocol
        // passthrough. `object` is fixed and re-emitted; `usage.total_tokens` is recomputed on write.
        let id = obj.get(keys::ID).and_then(|v| v.as_str()).map(String::from);
        let created = obj.get(CREATED).and_then(|v| v.as_u64());
        let system_fingerprint = obj
            .get(SYSTEM_FINGERPRINT)
            .and_then(|v| v.as_str())
            .map(String::from);

        // Per-token logprobs from the first choice, carried neutrally for a foreign caller.
        let logprobs =
            crate::codec::logprob_wire::read_token_logprobs(choices[0].get(keys::LOGPROBS));

        Ok(crate::codec::ir::IrResponse {
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
/// phase began (OAI-14). Tool keys are upstream `tool_calls[].index` values clamped to
/// `MAX_TOOL_INDEX`, so this key can never collide with a tool.
const LATE_THINKING_KEY: usize = usize::MAX;

/// The `open_tools` / `tool_ir_index` join key of a streamed legacy `delta.function_call` (OAI-08).
/// Real tool keys are clamped to `MAX_TOOL_INDEX`, so this can never collide with one.
const LEGACY_FUNCTION_CALL_KEY: usize = usize::MAX - 1;

/// The `tool_ir_index` key that records the IR index of the text block opened for `delta.refusal`
/// (IR-02). Never an `open_tools` member, so no tool close replays it.
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
/// `type` is not `function`. The reader is a tap and skips it rather than refuse the request, so a
/// same-dialect relay carries it byte for byte. A call that claims `function` (or names no type)
/// and carries none is still malformed.
fn unmodeled_tool_call(tc_val: &serde_json::Value) -> bool {
    tc_val
        .get(keys::TYPE)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|t| t != keys::FUNCTION)
}
