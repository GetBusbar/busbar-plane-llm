use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::ir_encode::{synth_response_tool_call_id, tool_input_from_arguments};

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
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;
        // Every count is read through `USAGE`: an unreadable one is refused alone and bills nothing,
        // the readable ones recover. A truncated body reads the same cache tiers as a whole one.
        Some(read_responses_usage(Some(&v), None).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse the error body once and pull both fields from the one tree (error paths are already degraded).
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

        // Native `/v1/responses` carries `code: "context_length_exceeded"`, but some upstreams (and the
        // Chat-shaped surface this proxy fronts) signal it only in the message with a null or generic
        // code. Mirror openai_chat.rs / anthropic.rs: with no canonical code, scan for the context-length
        // phrasing and synthesize it so the breaker classifies `StatusClass::ContextLength` and fails
        // over without penalizing the lane (counterpart of the `#[cfg(test)] classify()` scan).
        //
        // The scan is gated to 400/413, as in `OpenAiReader::extract_error`: otherwise a 401/429/5xx
        // whose prose says "maximum context length" would be misattributed and record no fault.
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
        // Both bearer-envelope dialects classify alike (shared dialect module).
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

        // Stateful Responses: `previous_response_id` and `store` have no cross-protocol target (the
        // upstream owns conversation state), so they ride `extra` verbatim (see
        // `responses_modeled_keys`) and re-emit unchanged on a same-protocol hop. Their types are
        // edge-validated here (string, bool), per the plane's "reject present-but-wrong-typed" policy,
        // since `extra` would otherwise relay a bad value to a backend that 400s. Absent or `null` stays lenient.
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
        // `system`/`developer` items folded out of `input` (the top-level `instructions` string is
        // not counted), keeping `message_count` the raw `input` length on `IrFacts::shape()`.
        let mut system_turns_folded: usize = 0;
        // Where each folded entry stood, for the hook view.
        let mut system_folds: Vec<crate::codec::ir::IrSystemFold> = Vec::new();
        // IR-14: roles the folded system entries used, and whether a role-less `instructions` was folded.
        let (mut saw_system, mut saw_developer, mut saw_instructions) = (false, false, false);

        if let Some(instructions) = obj.get(keys::INSTRUCTIONS).and_then(|v| v.as_str()) {
            if !instructions.is_empty() {
                saw_instructions = true;
                system_blocks.push(crate::codec::ir::IrBlock::text(instructions.to_string()));
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();

        if let Some(input_val) = obj.get(keys::INPUT) {
            // Edge-validate `input`: a bare string or an array. A present number/bool/object would be
            // coerced to an empty conversation, so reject with a 400 (as the openai_chat/cohere
            // readers do). `null` is tolerated as absent.
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
                        }
                        Some(INPUT_IMAGE) => {
                            // An `input_image` may reference an uploaded file by `file_id` instead of
                            // an inline `image_url`. Carry it as the typed file-id source so the writer
                            // restores `{type:input_image,file_id}`; `image_url` wins when present.
                            if let Some(block) = responses_input_image_block(item) {
                                messages.push(crate::codec::ir::IrMessage {
                                    role: crate::codec::ir::IrRole::User,
                                    content: vec![block],
                                });
                            }
                        }
                        Some(CONTENT_TYPE_OUTPUT_TEXT) => {
                            let text = item
                                .get(keys::TEXT)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            // Carry `annotations` (URL citations) on a top-level assistant `output_text`
                            // item into the Text block's citations, as the content-array path does.
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
                        }
                        Some(ITEM_TYPE_FUNCTION_CALL) => {
                            // A function-call item needs a non-empty string `call_id`, the key its
                            // `function_call_output` pairs against. Reject rather than invent one.
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
                            // `arguments` is a JSON string: parsed, or kept verbatim if malformed. A
                            // non-string (already-parsed) value is used directly, not collapsed to `{}`.
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
                        }
                        Some(FUNCTION_CALL_OUTPUT) => {
                            let call_id = item
                                .get(CALL_ID)
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output_val = item.get(keys::OUTPUT);
                            let content_blocks: Vec<crate::codec::ir::IrBlock> = match output_val {
                                Some(serde_json::Value::String(out_str)) => {
                                    vec![crate::codec::ir::IrBlock::text(out_str.clone())]
                                }
                                _ => output_val
                                    .and_then(|o| o.as_array())
                                    .map(|arr| {
                                        arr.iter().filter_map(|b| responses_block(b).ok()).collect()
                                    })
                                    .unwrap_or_default(),
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
                        }
                        Some(ITEM_TYPE_MESSAGE) => {
                            // Typed `{"type":"message","role":...,"content":[...]}` items are the SDK's
                            // turn form; the role-keyed fallback below only handles untyped items.
                            // Edge-validate `content` (string, array or absent/`null`): a present
                            // number/bool/object would vanish silently, so reject with a 400.
                            if let Some(cv) = item.get(keys::CONTENT) {
                                if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                                    return Err(ir_parse_error());
                                }
                            }
                            let role_str =
                                item.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");
                            // `system`/`developer` turns have no IrRole: their text goes into
                            // `system_blocks` (-> `IrRequest.system`), or the prompt is lost cross-protocol.
                            if role_str == keys::SYSTEM || role_str == keys::DEVELOPER {
                                // A `system`/`developer` item may appear anywhere in `input`, but the IR
                                // cannot express a positioned system turn and writers re-fold it to the
                                // top (the Anthropic API takes no mid-conversation system). It is hoisted
                                // to apply from turn 1, a real change after an earlier turn; warn.
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
                                push_system_content(&mut system_blocks, item.get(keys::CONTENT));
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
                                // `content` is a typed-block array or a bare string; both are handled.
                                if let Some(msg_content) =
                                    message_content_blocks(item.get(keys::CONTENT))
                                {
                                    messages.push(crate::codec::ir::IrMessage {
                                        role,
                                        content: msg_content,
                                    });
                                }
                            }
                        }
                        Some(ITEM_TYPE_REASONING) => {
                            // A prior-turn `reasoning` input item (text under `content`/`summary`,
                            // opaque blob under `encrypted_content`) becomes an `IrBlock::Thinking` on
                            // its own assistant turn, since it is a top-level sibling of the message it
                            // precedes. `write_request` re-emits it, so a Responses round-trip is kept.
                            let text = read_reasoning_text(item);
                            let signature =
                                read_reasoning_encrypted_content(item).map(String::from);
                            // The reasoning item's `id` (`rs_…`) has no IR slot and no other protocol
                            // models one, so it is dropped with a warn (only when present). The writer
                            // mints a fresh id; text and `encrypted_content` round-trip.
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
                                // IR-18: `encrypted_content` is an OpenAI-minted blob unless it is
                                // busbar's provenance envelope (another family's signature), which
                                // restores the original bytes and origin. IR-17: `content[]` vs
                                // `summary[]` says full vs summary.
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
                            }
                        }
                        Some(_) | None => {}
                    }

                    // Role/content items only when there is no `type`: a typed item that happens to
                    // carry a `role` must not be re-processed, or the turn is duplicated.
                    if item.get(keys::TYPE).is_none() && item.get(keys::ROLE).is_some() {
                        let role_str = item.get(keys::ROLE).and_then(|r| r.as_str()).unwrap_or("");
                        let content_val = item.get(keys::CONTENT);

                        // Edge-validate `content` as in the typed `message` arm.
                        if let Some(cv) = content_val {
                            if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                                return Err(ir_parse_error());
                            }
                        }

                        // Untyped `system`/`developer` turns go to `system_blocks` as in the typed arm.
                        if role_str == keys::SYSTEM || role_str == keys::DEVELOPER {
                            // Same hoist warn as the typed arm.
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
                            push_system_content(&mut system_blocks, content_val);
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

                        // Typed-block array or bare string, via `message_content_blocks`.
                        if let Some(msg_content) = message_content_blocks(content_val) {
                            messages.push(crate::codec::ir::IrMessage {
                                role,
                                content: msg_content,
                            });
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
            // A PRESENT `tools` that is not an array is a malformed request — reject it (mirroring the
            // `input` type-check) rather than coercing to empty, which would forward a tool-less
            // request upstream at HTTP 200 and silently strip the caller's tools.
            let tools_arr = tools_val.as_array().ok_or_else(ir_parse_error)?;
            for tool_val in tools_arr {
                // HOSTED-TOOL PASSTHROUGH. The Responses `tools` array mixes CUSTOM
                // function tools (`type:"function"` with a flat `name`/`parameters`) with provider-
                // HOSTED tools (`type:"web_search"`/`"file_search"`/`"code_interpreter"`/
                // `"computer_use_preview"`/`"mcp"`/...) that carry NO `name`/`parameters`. Parsing a
                // hosted tool as a function tool yielded an empty `{"type":"function","name":""}` that
                // a Responses backend 400s on. A tool is a function tool ONLY when its `type` is
                // exactly `"function"` (or, defensively, when `type` is absent but a `name` is present,
                // the pre-Responses shorthand); ANY other `type` is a hosted tool and must round-trip
                // its raw JSON verbatim so the writer re-emits it unchanged.
                let type_str = tool_val.get(keys::TYPE).and_then(|t| t.as_str());
                let is_function = match type_str {
                    Some(keys::FUNCTION) => true,
                    Some(_) => false,
                    None => tool_val.get(keys::NAME).is_some(),
                };
                if !is_function {
                    // IR-11: a hosted tool whose KIND (and every member) the IR models neutrally
                    // crosses the seam in `hosted_tools`; any other stays the raw same-protocol
                    // object below.
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
                    // Strict function calling, flat on a Responses tool (Chat nests it). Absent is
                    // `None`, never `Some(false)`.
                    strict: tool_val.get(keys::STRICT).and_then(|v| v.as_bool()),
                });
            }
        }

        // u64 with `try_from` so an out-of-range value becomes None instead of truncating (as the
        // anthropic and bedrock readers do). 0 is dropped.
        let max_tokens = obj
            .get(INCOMPLETE_REASON_MAX_OUTPUT)
            .and_then(|v| v.as_u64())
            .filter(|&v| v > 0)
            .and_then(|v| u32::try_from(v).ok());
        // `temperature` / `top_p` are map-file rows (read below). The API has no `top_k` or stop param.
        // `stream` is in the request body and is read.
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Promote `tool_choice` to the IR union so a forced directive survives the seam (it is a
        // modeled key, so it does not linger in `extra`). IR-10 (RSP-15): the `allowed_tools` form
        // yields the subset beside an Auto/Required choice.
        let (tool_choice, allowed_tools) = read_responses_tool_choice(obj.get(keys::TOOL_CHOICE));

        // response_format lives under `text.format` (not a top-level member): read and normalize it
        // to the IR's canonical shape. The create API models no `frequency_penalty`,
        // `presence_penalty`, `seed`, `n` or `stop` (verified against openai-python
        // `ResponseCreateParamsBase`), so those stay None/empty.
        let response_format = read_text_format(obj.get(keys::TEXT));

        // `text` is not in the modeled-keys set: its own branch below promotes `format` to
        // `response_format` and keeps the remainder in `extra`. `metadata` is also excluded; see
        // `responses_modeled_keys`.

        for (key, value) in obj.iter() {
            // `text` is partially modeled. Its `format` goes to `response_format` and must not linger
            // in `extra` (the writer rebuilds `text` from it, so a leftover would conflict), but other
            // sub-keys (e.g. `verbosity`) survive via a `format`-stripped copy in `extra`. Format-only
            // `text` is dropped from `extra`. Checked before the modeled-keys short-circuit.
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

        // The reasoning ask: promote the `reasoning.effort` word for Anthropic/Gemini budgets. The raw
        // object (which can carry `summary`) stays in `extra`; the writer uses the typed field only
        // when `extra` has no verbatim original.
        let reasoning = obj
            .get(keys::REASONING)
            .and_then(|r| r.get(keys::EFFORT))
            .and_then(|v| v.as_str())
            .and_then(read_responses_reasoning_effort);

        // `parallel_tool_calls` is modeled as on Chat (unlike Bedrock/Gemini/Cohere, where None is
        // the accurate "never said").
        let parallel_tool_calls = obj.get(keys::PARALLEL_TOOL_CALLS).and_then(|v| v.as_bool());

        // Top-level `top_logprobs` (0-20), as on Chat. Promoted first-class so it carries to OpenAI/
        // Gemini backends (which force the enabling `logprobs` from it) instead of riding `extra`,
        // which the seam clears without a drop-warn. `logprobs` stays `None`: there is no top-level
        // `logprobs` boolean here, and `top_logprobs` itself is the ask.
        let top_logprobs = obj
            .get(keys::TOP_LOGPROBS)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        // RSP-08: naming `message.output_text.logprobs` in `include` is the logprobs switch; carry it
        // as the IR `logprobs` ask. `include` stays in `extra` (it can name other members).
        let logprobs = obj
            .get(INCLUDE)
            .and_then(|i| i.as_array())
            .is_some_and(|arr| {
                arr.iter()
                    .any(|v| v.as_str() == Some(INCLUDE_OUTPUT_TEXT_LOGPROBS))
            })
            .then_some(true);

        // RSP-05: a string `user` is the same field Chat carries.
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
        _host: &dyn busbar_contract::codec::CodecHost,
        event_type: &str,
        data: &serde_json::Value,
        state: &mut crate::codec::ir::StreamDecodeState,
    ) -> Vec<IrStreamEvent> {
        let mut out: Vec<IrStreamEvent> = Vec::new();

        if !data.is_object() {
            return out;
        }

        match event_type {
            EVT_RESPONSE_CREATED | "response.in_progress" => {
                if !state.started {
                    state.started = true;
                    // Capture stream identity from the nested `response` so same-protocol passthrough
                    // keeps it. `created_at` maps to the IR's `created`.
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
                            // RSP-16: a blank `call_id` gets the same synthesized id the buffered
                            // `read_response` mints, since `output_index` is the ordinal it hashes.
                            let call_id = if raw_call_id.is_empty() {
                                synth_response_tool_call_id(
                                    usize::try_from(output_index).unwrap_or(usize::MAX),
                                    &name,
                                )
                            } else {
                                raw_call_id.to_string()
                            };
                            // Clamp the wire index before the cast (a crafted `u64::MAX` would feed
                            // unbounded index arithmetic); mirrors openai_chat.rs.
                            let idx = (output_index as usize).min(MAX_OUTPUT_INDEX);
                            // Record the open tool index so `output_item.done` closes it exactly once
                            // (a tool gets one `done`, unlike text which also gets `content_part.done`).
                            //
                            // Cap distinct-index cardinality (a unique `output_index` per event must
                            // not grow `open_tools` without bound). Open only when the index is not
                            // tracked and under the cap: a second `output_item.added` must not re-emit
                            // BlockStart (invalid BlockStart, BlockStart, BlockStop). Past the cap a
                            // new index is skipped, as in openai_chat.rs.
                            // An index must not open as both a tool and a text block (text uses
                            // `idx + TEXT_INDEX_KEY_OFFSET`): both would collapse onto IR index `idx`,
                            // duplicating `content_block_start` and BlockStop. So the text key must be
                            // clear before opening a tool block.
                            let already_open = state.open_tools.contains(&idx)
                                || state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET));
                            if !already_open && state.open_tools.len() < MAX_OPEN_TOOLS {
                                state.open_tools.insert(idx);
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
                    {
                        // REASONING: `output_item.added` typed `reasoning` opens a Thinking block at
                        // this `output_index`, tracked in `open_tools` at the raw idx and closed once by
                        // `output_item.done`. Same cap and already-open guard as the tool arm.
                        if let Some(output_index) = data.get(OUTPUT_INDEX).and_then(|i| i.as_u64())
                        {
                            let idx = (output_index as usize).min(MAX_OUTPUT_INDEX);
                            let already_open = state.open_tools.contains(&idx)
                                || state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET));
                            if !already_open && state.open_tools.len() < MAX_OPEN_TOOLS {
                                state.open_tools.insert(idx);
                                out.push(IrStreamEvent::BlockStart {
                                    index: idx,
                                    // IR-17: what the added item already says (usually unknown; its
                                    // arrays fill on later deltas).
                                    block: crate::codec::ir::IrBlockMeta::Thinking {
                                        kind: data.get(ITEM).and_then(super::slots::reasoning_kind),
                                    },
                                    refusal: false,
                                });
                            }
                        }
                    }
                }
            }

            // REASONING: `reasoning_text.delta` (full) and `reasoning_summary_text.delta` (summary)
            // become `IrDelta::ThinkingDelta` on the block at this `output_index`, lazily opening the
            // Thinking BlockStart when `output_item.added` was absent (some backends omit it). Tracked
            // at the raw idx in `open_tools`.
            EVT_REASONING_TEXT_DELTA | EVT_REASONING_SUMMARY_TEXT_DELTA => {
                let delta = data
                    .get(keys::DELTA)
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                if !delta.is_empty() {
                    let idx = data
                        .get(OUTPUT_INDEX)
                        .and_then(|i| i.as_u64())
                        .map_or(0, |v| (v as usize).min(MAX_OUTPUT_INDEX));
                    // Lazily open the block unless `output_item.added` did. Guard against a TEXT key
                    // collision at the same index and the cap; past the cap suppress the delta rather
                    // than emit an orphan.
                    if !state.open_tools.contains(&idx)
                        && !state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET))
                    {
                        if state.open_tools.len() >= MAX_OPEN_TOOLS {
                            return out;
                        }
                        state.open_tools.insert(idx);
                        // IR-17: the opening event says which reasoning array it fills.
                        let kind = if event_type == EVT_REASONING_TEXT_DELTA {
                            crate::codec::ir::IrThinkingKind::Full
                        } else {
                            crate::codec::ir::IrThinkingKind::Summary
                        };
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
                // Drop empty keepalive deltas: no block opens and no zero-length TextDelta is emitted.
                if !delta.is_empty() {
                    // The wire `output_index` is used for both the lazy BlockStart and the BlockDelta so
                    // open/close stay index-matched.
                    //
                    // ASSUMPTION: one `output_text` part per message item. The block is keyed on
                    // `output_index` alone, ignoring `content_index`, so two parts under one index would
                    // merge into one IR block. OpenAI ships exactly one today; if it ever ships
                    // multi-part items, key on (output_index, content_index) here and in the tool arm
                    // and finish-path close.
                    let idx = data
                        .get(OUTPUT_INDEX)
                        .and_then(|i| i.as_u64())
                        .map_or(0, |v| (v as usize).min(MAX_OUTPUT_INDEX));
                    // Open TEXT indices are tracked per index in `open_tools` under
                    // `TEXT_INDEX_KEY_OFFSET`, not a single `text_block_open` bool: a stream can carry
                    // several message items, and each new index gets its own lazy BlockStart rather than
                    // an orphan delta. The same cap applies; past it no BlockStart/BlockDelta is sent.
                    // Symmetric to the tool arm: an index already open as a TOOL block must not also open
                    // a TEXT block (duplicate `content_block_start`, later duplicate BlockStop).
                    let text_key = idx + TEXT_INDEX_KEY_OFFSET;
                    if state.open_tools.contains(&idx) {
                        // The index is held by an open TOOL block: drop the stray text delta, so no
                        // second block opens and no text fragment lands inside a tool-use block.
                        return out;
                    }
                    let already_open = state.open_tools.contains(&text_key);
                    if !already_open {
                        if state.open_tools.len() >= MAX_OPEN_TOOLS {
                            // Cap reached: suppress this index (no BlockStart, no orphan BlockDelta).
                            return out;
                        }
                        state.open_tools.insert(text_key);
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
                    // RSP-04: the delta's `logprobs[]` (`ResponseLogProb`) ride the same text block as
                    // a `LogprobsDelta`, as the buffered read carries them onto `IrResponse.logprobs`.
                    // Empty or absent emits nothing.
                    let lps = read_responses_logprobs(data.get(keys::LOGPROBS));
                    if !lps.is_empty() {
                        out.push(IrStreamEvent::BlockDelta {
                            index: idx,
                            delta: crate::codec::ir::IrDelta::LogprobsDelta(lps),
                        });
                    }
                }
            }

            // Streamed citations: each `url_citation` arrives as its own
            // `output_text.annotation.added` frame, after the text deltas and before
            // `output_text.done`. It is read through the same `read_url_annotations` as the buffered
            // path and carried as `IrDelta::CitationsDelta` on the open text block at this index. An
            // annotation for an index that is not an open text block is dropped, not orphaned.
            EVT_OUTPUT_TEXT_ANNOTATION_ADDED => {
                let idx = data
                    .get(OUTPUT_INDEX)
                    .and_then(|i| i.as_u64())
                    .map_or(0, |v| (v as usize).min(MAX_OUTPUT_INDEX));
                if state.open_tools.contains(&(idx + TEXT_INDEX_KEY_OFFSET)) {
                    if let Some(annotation) = data.get("annotation") {
                        let citations = super::super::url_citation_wire::read_url_annotations(
                            &serde_json::Value::Array(vec![annotation.clone()]),
                        );
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
                if !delta.is_empty() {
                    if let Some(output_index) = data.get(OUTPUT_INDEX).and_then(|i| i.as_u64()) {
                        let idx = (output_index as usize).min(MAX_OUTPUT_INDEX);
                        // Route argument deltas only to an index that emitted a BlockStart (in
                        // `open_tools`). A cap-suppressed index, or deltas with no preceding
                        // `output_item.added`, would produce a fragment with no `content_block_start`
                        // that breaks strict SDKs. Mirrors openai_chat.rs's guard.
                        if state.open_tools.contains(&idx) {
                            out.push(IrStreamEvent::BlockDelta {
                                index: idx,
                                delta: crate::codec::ir::IrDelta::InputJsonDelta(delta),
                            });
                        }
                    }
                }
            }

            EVT_OUTPUT_ITEM_DONE | EVT_CONTENT_PART_DONE => {
                if let Some(output_index) = data.get(OUTPUT_INDEX).and_then(|i| i.as_u64()) {
                    let idx = (output_index as usize).min(MAX_OUTPUT_INDEX);
                    // Native Responses closes a text item with two terminal frames at one
                    // `output_index` (`content_part.done`, then `output_item.done`). A BlockStop for
                    // both would duplicate `content_block_stop`, so close exactly once: only for an
                    // index currently open, clearing the marker so the second frame is a no-op. Tool
                    // (raw `idx`) and text (`TEXT_INDEX_KEY_OFFSET`) indices are tracked per index in
                    // `open_tools`, so the close routes to the right block.
                    //
                    // A raw index (function call or reasoning) closes on `output_item.done` only: a
                    // reasoning item's `content_part.done` arrives first, and `output_item.done`
                    // carries its `encrypted_content`, which must be read before closing.
                    let item_done = event_type == EVT_OUTPUT_ITEM_DONE;
                    if item_done && state.open_tools.remove(&idx) {
                        // RSP-02: a reasoning item's `encrypted_content` (multi-turn reuse blob) rides
                        // this frame. Carry it as a `SignatureDelta` before the BlockStop, as the
                        // buffered read does (rule shared via `read_reasoning_encrypted_content`).
                        if let Some(sig) = data
                            .get(ITEM)
                            .filter(|it| {
                                it.get(keys::TYPE).and_then(|t| t.as_str())
                                    == Some(ITEM_TYPE_REASONING)
                            })
                            .and_then(read_reasoning_encrypted_content)
                        {
                            out.push(IrStreamEvent::BlockDelta {
                                index: idx,
                                delta: crate::codec::ir::IrDelta::SignatureDelta(sig.to_string()),
                            });
                        }
                        // A now-closed function-call or reasoning item.
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.open_tools.remove(&(idx + TEXT_INDEX_KEY_OFFSET)) {
                        // An open text block: close this index once. Removing the per-index key lets
                        // other text indices stay open and makes the paired second terminal frame a no-op.
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    }
                    // Otherwise nothing is open here (second terminal frame, or a `done` for an
                    // unopened item): emit nothing.
                }
            }

            EVT_RESPONSE_COMPLETED | EVT_RESPONSE_FAILED | EVT_RESPONSE_INCOMPLETE => {
                // A terminal event ends the message. Blocks still open (upstream cut off mid-block, or
                // `failed`/`incomplete` mid-stream) are drained from `open_tools` and given a BlockStop
                // before MessageStop, converting text keys back to IR indices; otherwise a strict SDK
                // rejects the unbalanced stream. Invoked on every terminal path, including the failed
                // early-return.
                if let Some(response_obj) = data.get(keys::RESPONSE) {
                    let status = response_obj
                        .get(keys::STATUS)
                        .and_then(|s| s.as_str())
                        .unwrap_or("");

                    // A genuinely failed terminal stream must NOT be decoded as a successful
                    // end_turn — that would mask the upstream failure from a downstream client
                    // (e.g. an Anthropic client would see stop_reason=end_turn). Surface it as an
                    // explicit IrStreamEvent::Error so the failure propagates, then still terminate
                    // the stream so consumers do not hang.
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
                        // Derive the breaker class from the captured provider signal rather than
                        // hardcoding ServerError: an auth/rate-limit/context-length failure that
                        // arrives mid-stream must classify the same way it would on the non-stream
                        // HTTP path, or the breaker takes the wrong disposition/failover. The
                        // fallback "response_failed" (no error code/type present) maps to the
                        // default ServerError bucket.
                        let class = class_for_response_failed(
                            provider_signal.as_deref().unwrap_or(SIGNAL_RESPONSE_FAILED),
                        );
                        // Billed counts: a failed response that still reports its usage (the
                        // tokens the far end generated before it failed) bills them (a cut is
                        // not a refund). A present-but-unreadable count refuses in place of
                        // the failure, exactly as the completed arm below does.
                        // A native failed response usually says `"usage": null`: nothing reported,
                        // nothing to carry.
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
                        // The usage rides a `MessageDelta{Error}` after the Error, as in the Cohere
                        // `ERROR` and Gemini `MALFORMED_FUNCTION_CALL` arms: the translator folds it
                        // into billing and writers read a delta after an Error as the end (RSP-11).
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

                    // Enumerate the known statuses; an unrecognized one is a terminal stop with
                    // reason None, never a claimed success.
                    let stop_reason = match status {
                        STATUS_COMPLETED | "" => Some(crate::codec::ir::IrStopReason::EndTurn),
                        // `incomplete` is not a clean end_turn: map its reason, or None (don't mask truncation).
                        STATUS_INCOMPLETE => response_obj
                            .get(INCOMPLETE_DETAILS)
                            .and_then(|d| d.get(keys::REASON))
                            .and_then(|r| r.as_str())
                            .map(read_responses_incomplete_reason),
                        _ => None,
                    };

                    // Tool-use override, as in `read_response`: a `completed` end_turn becomes
                    // `tool_use` when the fully assembled `output` carries a function_call, so
                    // streamed tool calls give cross-protocol clients the finish signal.
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

                    // Refusal override, as in `read_response`. A streamed refusal arrives only in this
                    // terminal frame as an `output[].content[]` `{type:"refusal"}` part (status stays
                    // `completed`), never via `output_text.delta`. Emit it as a Text block (closed by
                    // `close_open_blocks`) and promote end_turn to Refusal. Anthropic/Bedrock have no
                    // refusal part: plain text plus a `refusal` stop.
                    let mut saw_refusal = false;
                    if let Some(items) = response_obj.get(keys::OUTPUT).and_then(|o| o.as_array()) {
                        for (item_pos, item) in items.iter().enumerate() {
                            let Some(content) = item.get(keys::CONTENT).and_then(|c| c.as_array())
                            else {
                                continue;
                            };
                            for block in content {
                                if block.get(keys::TYPE).and_then(|t| t.as_str())
                                    != Some(keys::REFUSAL)
                                {
                                    continue;
                                }
                                let Some(text) = block
                                    .get(keys::REFUSAL)
                                    .and_then(|r| r.as_str())
                                    .filter(|s| !s.is_empty())
                                else {
                                    continue;
                                };
                                saw_refusal = true;
                                // Open a Text block at this item's index unless already open
                                // (defensive) or the per-stream block cap is reached.
                                let idx = item_pos.min(MAX_OUTPUT_INDEX);
                                let text_key = idx + TEXT_INDEX_KEY_OFFSET;
                                if !state.open_tools.contains(&idx)
                                    && !state.open_tools.contains(&text_key)
                                    && state.open_tools.len() < MAX_OPEN_TOOLS
                                {
                                    state.open_tools.insert(text_key);
                                    // IR-02: the block is the refusal, as the buffered read says.
                                    out.push(IrStreamEvent::BlockStart {
                                        index: idx,
                                        block: crate::codec::ir::IrBlockMeta::Text,
                                        refusal: true,
                                    });
                                    out.push(IrStreamEvent::BlockDelta {
                                        index: idx,
                                        delta: crate::codec::ir::IrDelta::TextDelta(
                                            text.to_string(),
                                        ),
                                    });
                                }
                            }
                        }
                    }
                    let stop_reason = if saw_refusal
                        && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn)
                    {
                        Some(crate::codec::ir::IrStopReason::Refusal)
                    } else {
                        stop_reason
                    };

                    // Billed counts: absent is zero; a present-but-unreadable count ends the stream in
                    // an error instead of ledgering zero tokens.
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

                    // Close open blocks before the MessageDelta: BlockStop*, MessageDelta, MessageStop,
                    // as Anthropic sequences them.
                    close_open_blocks(&mut out, state);
                    out.push(IrStreamEvent::MessageDelta {
                        stop_reason,
                        // The Responses stream has no stop_sequence analog.
                        stop_sequence: None,
                        usage,
                        stop_detail: None,
                    });
                    out.push(IrStreamEvent::MessageStop);
                } else if event_type == EVT_RESPONSE_FAILED {
                    // Terminal failure with no nested `response` (truncated SSE frame, or a proxy
                    // stripped the body). The wire `event_type` is the only signal; a successful
                    // end_turn would mask the failure and deny the breaker its signal. Emit Error then
                    // MessageStop, classified through the shared helper (which maps the unrecognized
                    // "response_failed" sentinel to ServerError) like the body-present arm.
                    let provider_signal = SIGNAL_RESPONSE_FAILED;
                    out.push(IrStreamEvent::Error(IrError {
                        class: class_for_response_failed(provider_signal),
                        provider_signal: Some(provider_signal.to_string()),
                        retry_after: None,
                    }));
                    close_open_blocks(&mut out, state);
                    out.push(IrStreamEvent::MessageStop);
                } else {
                    // Terminal completed/incomplete with no nested `response`: still end the stream
                    // with MessageDelta + MessageStop so consumers do not hang. The stop reason comes
                    // from the event name; a bodyless `incomplete` has no reason to surface, so None
                    // (as in the body-present arm and `read_response`). Only `completed` is end_turn.
                    // `failed` is handled above.
                    let stop_reason = (event_type == EVT_RESPONSE_COMPLETED)
                        .then_some(crate::codec::ir::IrStopReason::EndTurn);
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
        _host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        let status = obj.get(keys::STATUS).and_then(|s| s.as_str()).unwrap_or("");

        // A body with `status:"failed"` is an upstream provider failure, not a parse failure. The
        // writer emits `{"status":"failed","output":[],"error":{...}}` (a present empty array), so
        // handle it before the `output` branch or the empty array would fail the usage check and
        // surface as an `ir_parse` ClientFault (wrong breaker transition). Whether `output` is `[]`,
        // null or absent, surface the upstream signal as the streaming `response.failed` arm does:
        // `error.code`, else `error.type`, else a generic `response_failed`.
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
            // Derive the breaker class from the captured signal, as the streaming arms do, not a
            // hardcoded ServerError.
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
        // A refusal rides a `refusal` content part with `status:"completed"`; tracked here to
        // promote `stop_reason` to `Refusal` below.
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
                                        // `annotations` is a sibling key on this part; see
                                        // `read_url_annotations` for why offsets are not carried.
                                        let mut citations = block_item
                                            .get(keys::ANNOTATIONS)
                                            .map(super::super::url_citation_wire::read_url_annotations)
                                            .unwrap_or_default();
                                        citations.extend(super::read_file_annotations(
                                            block_item.get(keys::ANNOTATIONS),
                                        ));
                                        // RSP-03: part `logprobs` join the response's one IR logprob
                                        // run in part order; the writer attaches that run to the first part.
                                        logprobs.extend(read_responses_logprobs(
                                            block_item.get(keys::LOGPROBS),
                                        ));
                                        content.push(crate::codec::ir::IrBlock::Text {
                                            text: text.to_string(),
                                            cache_control: None,
                                            citations,
                                            refusal: false,
                                        });
                                    }
                                } else if block_type == keys::REFUSAL {
                                    // A model refusal rides a `{type:"refusal"}` content part. Carried
                                    // as assistant Text so the explanation survives (Anthropic/Bedrock
                                    // have no refusal part); the signal is promoted onto `stop_reason`
                                    // below.
                                    if let Some(text) =
                                        block_item.get(keys::REFUSAL).and_then(|t| t.as_str())
                                    {
                                        saw_refusal = true;
                                        // IR-02: flagged, so Chat/Responses clients get it in their refusal slot.
                                        content.push(crate::codec::ir::IrBlock::Text {
                                            text: text.to_string(),
                                            cache_control: None,
                                            citations: Vec::new(),
                                            refusal: true,
                                        });
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
                        // The `call_id` is the key a later `function_call_output` and cross-protocol
                        // targets (Anthropic `tool_use.id`) pair against, and a blank one is rejected.
                        // Rather than fail an otherwise good body, synthesize a deterministic `call_…`
                        // id when the backend supplied none.
                        let raw_call_id = item.get(CALL_ID).and_then(|c| c.as_str()).unwrap_or("");
                        let call_id = if raw_call_id.is_empty() {
                            synth_response_tool_call_id(item_ordinal, &name)
                        } else {
                            raw_call_id.to_string()
                        };
                        // `arguments` is a JSON string (malformed kept verbatim); a non-string value is
                        // used directly, not collapsed to `{}`.
                        let input = tool_input_from_arguments(item.get(keys::ARGUMENTS));

                        content.push(crate::codec::ir::IrBlock::ToolUse {
                            id: call_id,
                            name,
                            input,
                            cache_control: None,
                            thought_signature: None,
                        });
                    }

                    // REASONING: a `reasoning` output item becomes an `IrBlock::Thinking`. The text is
                    // in `content[].text` (`reasoning_text`) and/or `summary[].text` (`summary_text`),
                    // concatenated. `encrypted_content` (multi-turn reuse blob) maps to the IR
                    // `signature` slot so a same-protocol round-trip preserves it.
                    //
                    // The slot is one opaque token shared across protocols, but each vendor's is
                    // vendor-scoped (Anthropic `signature`, Responses `encrypted_content`, Gemini
                    // `thoughtSignature`): the bytes survive the seam, yet a foreign vendor's blob is
                    // not re-feedable to the other API, so cross-vendor reasoning reuse is unsupported.
                    ITEM_TYPE_REASONING => {
                        let text = read_reasoning_text(item);
                        let signature = read_reasoning_encrypted_content(item).map(String::from);
                        // Skip a wholly empty reasoning item (no text, no `encrypted_content`).
                        if !text.is_empty() || signature.is_some() {
                            // IR-18: `encrypted_content` is OpenAI-minted (a busbar provenance
                            // envelope restores the bytes and origin). IR-17: `content[]` vs
                            // `summary[]` says full vs summary.
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
                        }
                    }

                    // A provider-run web search: the IR hosted web-search record, sources as results.
                    super::ITEM_TYPE_WEB_SEARCH_CALL => {
                        content.push(super::read_web_search_call(item));
                    }
                    // Other hosted-tool output items (`file_search_call`, `code_interpreter_call`,
                    // `computer_call`, `mcp_call`, ...) have no neutral IR representation or
                    // cross-protocol analog, so they are dropped with a warn naming the type, never
                    // silently. On a translate attempt this goes through the one drop path (WARN plus
                    // audit row `output[].type=<item>`); a same-dialect relay says nothing. The
                    // message/tool-call/reasoning output is still carried by the arms above.
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
            // `status:"failed"` returned early, so a missing or non-array `output` is a parse failure.
            return Err(ir_parse_error());
        }

        // Promote a clean `end_turn` to `tool_use` when the content carries a tool call, as the
        // streaming arm does. Only `end_turn` is promoted: for an `incomplete` status a surviving
        // partial function_call must not clobber `max_tokens`/`safety` and hide the truncation.
        if stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn)
            && content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolUse { .. }))
        {
            stop_reason = Some(crate::codec::ir::IrStopReason::ToolUse);
        }

        // A `completed` response with a `refusal` part is a refusal: promote the typed `Refusal`.
        if saw_refusal && stop_reason == Some(crate::codec::ir::IrStopReason::EndTurn) {
            stop_reason = Some(crate::codec::ir::IrStopReason::Refusal);
        }

        // An absent `usage` is zero-defaulted, as in the sibling readers: it is an upstream quirk
        // (mock/staging/proxy), and an error here would make the engine discard a valid body.
        let usage_val = obj.get(keys::USAGE);

        // Absent is zero; a present-but-unreadable count refuses. See `USAGE` for cache
        // normalization; the serving tier is the top-level `service_tier`.
        let usage = read_responses_usage(usage_val, Some(body))?;

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        // Capture identity so responses-to-responses passthrough keeps `id`/`created_at`. The API
        // names the timestamp `created_at`, mapped to the IR `created`; `system_fingerprint` and
        // `stop_sequence` have no analog and stay `None`.
        let id = obj.get(keys::ID).and_then(|i| i.as_str()).map(String::from);
        let created = obj.get(keys::CREATED_AT).and_then(|c| c.as_u64());

        // A response echoes the request's `instructions` and `metadata`. `IrResponse` models neither
        // and no other protocol echoes them, so they are dropped on read with a warn (only when
        // present). The request side carries them separately (system blocks / `extra`).
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
        // The response also echoes `previous_response_id` and `store`. The stateful linkage (this
        // `id` as the next request's `previous_response_id`) is preserved by `IrResponse.id`; the
        // echo is not reconstructed, so it is dropped with a warn (only when present).
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
