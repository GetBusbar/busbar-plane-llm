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

    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;
        // The usage object is read as in `read_response` (one table, `USAGE`), so a head-truncated body
        // ledgers what its whole twin does: the raw `tokens` counts, with `billed_units` beside them as
        // attribution (`to_token_usage`). An unreadable count is refused alone and bills nothing.
        Some(read_cohere_usage(Some(&v)).ok()?.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse once and derive both fields from the one binding, as the Gemini and Bedrock readers do.
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

        // Cohere v2 signals an oversized request via the error message only (its `error_type` is the
        // generic `invalid_request_error`, the message free text such as "too many tokens"), so
        // `provider_code` would carry that raw string, which the breaker cannot recognize: the 400
        // would be a plain ClientError that never fails over. The `#[cfg(test)] classify()` helper
        // synthesized the canonical code but does not run in production. Mirror
        // `AnthropicReader::extract_error`: scan for Cohere's context-length phrasing and override
        // `provider_code` with `context_length_exceeded` (breaker.rs `normalize_raw_error`, then
        // `StatusClass::ContextLength`, failing over without penalty). Cohere always populates
        // `provider_code` from `message`, so the canonical code must replace it, not fill an empty slot.
        // The scan is gated to a request-size status (400, and 413 for some gateways): a 401/403/429
        // whose text mentions token counts must not become the no-penalty ContextLength class, which
        // would let an auth or rate-limit failure escape the breaker.
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

        let mut extra = serde_json::Map::new();
        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        // `system`-role entries folded out of `messages`, keeping `message_count` the raw wire array length.
        let mut system_turns_folded: usize = 0;
        // Where each folded entry stood, for the hook view (`IrRequest::system_folds`).
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

                // Edge-validate `content` (all roles): legally a string, an array of parts, or absent/`null`
                // (an assistant turn with only `tool_calls`). A present number/bool/object would be
                // silently coerced, so reject with the same 400 as the top-level `messages`.
                if let Some(cv) = msg_val.get(keys::CONTENT) {
                    if !cv.is_null() && !cv.is_string() && !cv.is_array() {
                        return Err(ir_parse_error());
                    }
                }

                // System content goes into IrRequest.system (as in the other protocols), so it survives
                // translation to a protocol whose writer reads req.system.
                if role == crate::codec::ir::IrRole::System {
                    system_turns_folded += 1;
                    let blocks_before = system_blocks.len();
                    if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if let Some(s) = content_val.as_str() {
                            system_blocks.push(crate::codec::ir::IrBlock::text(s.to_string()));
                        } else if let Some(arr) = content_val.as_array() {
                            for block_val in arr {
                                if let Some(bo) = block_val.as_object() {
                                    if bo.get(keys::TYPE).and_then(|t| t.as_str())
                                        == Some(keys::TEXT)
                                    {
                                        if let Some(text) =
                                            bo.get(keys::TEXT).and_then(|t| t.as_str())
                                        {
                                            system_blocks.push(crate::codec::ir::IrBlock::text(
                                                text.to_string(),
                                            ));
                                        }
                                    } else {
                                        // Cohere's system message is text-only, so a non-text block has no
                                        // representation and is dropped, but surfaced (a silent loss of a
                                        // system instruction block is invisible).
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
                // The generic content loop must not run for the Tool role: tool content is consumed
                // by the dedicated Tool branch below into the ToolResult. Running it too decoded the same
                // `content` into stray top-level Text blocks, so one tool message produced both a Text
                // block and a ToolResult with identical text, which CohereWriter's Tool branch then
                // folded into the first ToolResult, duplicating it. The Tool branch owns a tool
                // message's content (as the System early `continue` does).
                if role != crate::codec::ir::IrRole::Tool {
                    if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if content_val.is_string() {
                            msg_content.push(crate::codec::ir::IrBlock::text(
                                content_val.as_str().unwrap_or("").to_string(),
                            ));
                        } else if let Some(arr) = content_val.as_array() {
                            for block_val in arr {
                                if let Some(block_obj) = block_val.as_object() {
                                    match block_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
                                        Some(keys::TEXT) => {
                                            if let Some(text) =
                                                block_obj.get(keys::TEXT).and_then(|t| t.as_str())
                                            {
                                                msg_content.push(crate::codec::ir::IrBlock::text(
                                                    text.to_string(),
                                                ));
                                            }
                                        }
                                        // Cohere v2 multimodal input: an image part
                                        // `{"type":"image_url","image_url":{"url":"<data-uri|https>"}}`,
                                        // the OpenAI-compatible shape. Decoded into `IrBlock::Image` via
                                        // the shared `parse_image_url` (a `data:<mime>;base64,<payload>`
                                        // URI splits into (media_type, data); a raw URL is kept), the same
                                        // contract as the Anthropic/OpenAI readers. ASSUMPTION: no Cohere v2
                                        // image fixture exists in-repo to confirm the shape.
                                        Some(keys::IMAGE_URL) => {
                                            if let Some(url) = block_obj
                                                .get(keys::IMAGE_URL)
                                                .and_then(|iu| iu.get(keys::URL))
                                                .and_then(|u| u.as_str())
                                            {
                                                msg_content.push(crate::codec::ir::IrBlock::Image {
                                                    source:
                                                        super::super::ir_encode::parse_image_url(
                                                            url,
                                                        ),
                                                    cache_control: None,
                                                    // IR-08: the requested fidelity.
                                                    detail: read_cohere_image_detail(
                                                        block_obj.get(keys::IMAGE_URL),
                                                    ),
                                                });
                                            }
                                        }
                                        // A `{"type":"document","document":{id?, data}}` part (COH-04):
                                        // read into the IR's document Media (see `read_cohere_document`).
                                        Some(keys::DOCUMENT) => {
                                            if let Some(doc) = block_obj.get(keys::DOCUMENT) {
                                                msg_content.push(read_cohere_document(doc));
                                            }
                                        }
                                        // An assistant turn replaying a reasoning model's
                                        // `{"type":"thinking"}` part: the IR Thinking block in place (COH-04).
                                        Some(keys::THINKING)
                                            if role == crate::codec::ir::IrRole::Assistant =>
                                        {
                                            if let Some(text) = block_obj
                                                .get(keys::THINKING)
                                                .and_then(|t| t.as_str())
                                                .filter(|s| !s.is_empty())
                                            {
                                                msg_content.push(
                                                    crate::codec::ir::IrBlock::Thinking {
                                                        text: text.to_string(),
                                                        signature: None,
                                                        redacted: false,
                                                        cache_control: None,
                                                        kind: None,
                                                        signature_origin: None,
                                                    },
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }

                // A request assistant message may replay the pre-tool-call plan in `tool_plan` (the
                // history counterpart of the response `message.tool_plan`). Read it into a leading
                // Thinking block, as `read_response` does, so it is neither dropped from history nor
                // mistaken for visible content. The writer reshapes it back into `tool_plan` on egress.
                if role == crate::codec::ir::IrRole::Assistant {
                    if let Some(plan) = msg_val.get(TOOL_PLAN).and_then(|p| p.as_str()) {
                        if !plan.is_empty() {
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
                            );
                        }
                    }
                }

                // A request message may carry grounding `citations` (replayed prior grounded turn).
                // They attach to the first text block (Cohere offsets index the assembled content text,
                // which every writer's join starts from), neither dropped nor multiplied, through the
                // same `read_cohere_citations` seam as the response.
                if role != crate::codec::ir::IrRole::Tool {
                    if let Some(cits_val) = msg_val.get(keys::CITATIONS) {
                        let cits = super::read_cohere_citations(cits_val);
                        if !cits.is_empty() {
                            if let Some(crate::codec::ir::IrBlock::Text { citations, .. }) =
                                msg_content
                                    .iter_mut()
                                    .find(|b| matches!(b, crate::codec::ir::IrBlock::Text { .. }))
                            {
                                *citations = cits;
                            } else {
                                // COH-17 request side: a grounded turn with no text part carries its
                                // citations on an empty text block, as the response reader does; a
                                // dialect that rejects empty text (Anthropic, Bedrock, Gemini) omits it.
                                msg_content.push(crate::codec::ir::IrBlock::Text {
                                    text: String::new(),
                                    cache_control: None,
                                    citations: cits,
                                    refusal: false,
                                });
                            }
                        }
                    }
                }

                if role == crate::codec::ir::IrRole::Assistant {
                    if let Some(tool_calls) = msg_val.get(keys::TOOL_CALLS) {
                        if let Some(tc_arr) = tool_calls.as_array() {
                            for tc_val in tc_arr {
                                if let Some(func_obj) = tc_val.get(keys::FUNCTION) {
                                    // A tool call needs a non-empty string `id`, the key the egress
                                    // dialect uses to pair the tool result. Reject rather than invent one.
                                    let id = tc_val
                                        .get(keys::ID)
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                        .ok_or_else(ir_parse_error)?
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
                                    let input = crate::codec::json::parse_str(arguments).unwrap_or(
                                        serde_json::Value::String(arguments.to_string()),
                                    );
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
                }

                if role == crate::codec::ir::IrRole::Tool {
                    let tool_call_id = msg_val
                        .get(keys::TOOL_CALL_ID)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content_text = if let Some(content_val) = msg_val.get(keys::CONTENT) {
                        if let Some(arr) = content_val.as_array() {
                            // Tool content is an array (bare strings are accepted). The native shape is
                            // typed objects (`[{"type":"text",...}]`, `[{"type":"document",...}]`): pull
                            // `text` from text blocks and JSON-serialize other typed blocks so their
                            // content is preserved, mirroring the user/assistant text decoding above.
                            arr.iter()
                                .filter_map(|b| {
                                    if let Some(s) = b.as_str() {
                                        Some(s.to_string())
                                    } else if let Some(bo) = b.as_object() {
                                        if bo.get(keys::TYPE).and_then(|t| t.as_str())
                                            == Some(keys::TEXT)
                                        {
                                            bo.get(keys::TEXT)
                                                .and_then(|t| t.as_str())
                                                .map(String::from)
                                        } else if bo.contains_key(keys::DOCUMENT) {
                                            // A `document` part is captured structurally below
                                            // (`doc_blocks`); serializing it into this text join once made
                                            // the model see escaped JSON syntax instead of the document.
                                            // Nothing here.
                                            None
                                        } else {
                                            // Preserve any other non-text typed block verbatim.
                                            crate::codec::json::to_string(b).ok()
                                        }
                                    } else {
                                        // Non-string, non-object array element: serialized so nothing is lost.
                                        crate::codec::json::to_string(b).ok()
                                    }
                                })
                                .collect::<Vec<_>>()
                                // Concatenated with no separator: the OpenAI/Anthropic writers join text
                                // blocks with `""`, so a space would corrupt content split across blocks on
                                // a Cohere to OpenAI to Cohere round trip (a phantom space per boundary).
                                .join("")
                        } else if let Some(s) = content_val.as_str() {
                            s.to_string()
                        } else {
                            crate::codec::json::to_string(content_val).unwrap_or_default()
                        }
                    } else {
                        String::new()
                    };
                    let mut result_content: Vec<crate::codec::ir::IrBlock> = Vec::new();
                    if !content_text.is_empty() {
                        result_content.push(crate::codec::ir::IrBlock::text(content_text));
                    }
                    // `document` parts are captured structurally, not folded into the text join (which
                    // made the model read escaped JSON). A tool-result document is the tool's output: its
                    // JSON `data` reads into the IR `Json` block (string `data` into `Text`), the carrier
                    // every writer projects in a tool result. It once rode the opaque cohere `Vendor`
                    // escape every foreign writer drops, so an Anthropic backend saw an empty tool result
                    // (the ANT-17 follow-up). The document `id` is a Cohere citation handle with no
                    // foreign analog; only a document with no readable `data` keeps the `Vendor` escape.
                    if let Some(arr) = msg_val.get(keys::CONTENT).and_then(|c| c.as_array()) {
                        for b in arr {
                            let Some(doc) = b.get(keys::DOCUMENT) else {
                                continue;
                            };
                            result_content.push(read_cohere_tool_result_document(doc));
                        }
                    }
                    // A tool result with no parts still needs one empty text block: writers project its
                    // content, and an empty vec would emit a result with no content.
                    if result_content.is_empty() {
                        result_content.push(crate::codec::ir::IrBlock::text(String::new()));
                    }
                    msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                        tool_use_id: tool_call_id,
                        content: result_content,
                        is_error: false,
                        cache_control: None,
                    });
                }

                messages.push(crate::codec::ir::IrMessage {
                    role,
                    content: msg_content,
                });
            }
        } else {
            return Err(ir_parse_error());
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        if let Some(tools_arr) = obj.get(keys::TOOLS).and_then(|v| v.as_array()) {
            for tool_val in tools_arr {
                if let Some(func_obj) = tool_val.get(keys::FUNCTION) {
                    let name = func_obj
                        .get(keys::NAME)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
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
        }

        // `u32::try_from`, not `as u32`: a `max_tokens` above `u32::MAX` would wrap to a nonsense cap
        // forwarded to Cohere. Out of range is `None`, as in the Gemini reader; the `v > 0` filter
        // rejects zero/negative first.
        let max_tokens = obj
            .get(keys::MAX_TOKENS)
            .and_then(|v| v.as_i64())
            .filter(|&v| v > 0)
            .and_then(|v| u32::try_from(v).ok());
        // temperature, `p`, `k`, stop_sequences, penalties and seed are map-file rows, read below.
        // Cohere v2 `tool_choice` is a top-level enum string (REQUIRED/NONE), promoted to the IR union.
        let tool_choice = read_cohere_tool_choice(obj.get(keys::TOOL_CHOICE));
        let stream = obj
            .get(keys::STREAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Cohere v2 chat models `response_format` (json_object / json_schema) at the top level, carried verbatim into the IR.
        let response_format = obj
            .get(keys::RESPONSE_FORMAT)
            .and_then(read_cohere_response_format);

        // Request `logprobs` is a top-level boolean ask, the same meaning as OpenAI's `logprobs` and
        // Gemini's `responseLogprobs`. Promoted to `IrRequest.logprobs` so the ask survives the seam
        // instead of dying in `extra` (and it is in the modeled-key set so a same-protocol passthrough
        // does not double-emit). Response-side logprobs data is separate (see `read_response`).
        let logprobs = obj.get(keys::LOGPROBS).and_then(|v| v.as_bool());

        // Keep unmodelled members in `extra`. (The modeled set is static data in the map, built once, not per request.)
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        // The reasoning ask: `thinking: {type:"enabled", token_budget:N}` is the IR `Budget` (Anthropic
        // `budget_tokens` / Gemini `thinkingBudget`); `{type:"enabled"}` with no budget is `Dynamic`;
        // `{type:"disabled"}` is `Off` (IR-09). It once rode `extra` and died at the seam, so a caller's
        // reasoning ask never reached a foreign reasoning model (COH-05). An unreadable form stays in
        // `extra`, as in the Anthropic reader.
        // `strict_tools` is Cohere's strict-schema switch for every tool: the IR carries it per tool
        // (`IrTool.strict`), set on each (COH-11). It once rode `extra` and died at the seam, turning
        // the guarantee off on a foreign backend. Promoted only when there are tools; a bare flag stays in `extra`.
        // `strict_tools` is Cohere's strict-schema switch for EVERY tool of the request: the IR
        // carries the same guarantee per tool (`IrTool.strict`, OpenAI `tools[].function.strict`),
        // so it is set on each tool (COH-11). It used to ride `extra` and die at the seam, turning
        // the caller's schema guarantee off on a foreign backend. Promoted only when there are
        // tools to carry it; a bare flag stays in `extra`.
        if !tools.is_empty() {
            if let Some(strict) = obj.get(STRICT_TOOLS).and_then(|v| v.as_bool()) {
                for tool in &mut tools {
                    tool.strict = Some(strict);
                }
                extra.remove(STRICT_TOOLS);
            }
        }

        // Top-level `documents` (RAG grounding) read into the IR document `Media` (IR-13), ahead of the
        // first user turn's content: grounding is conversation-level and the first turn is where every
        // foreign document slot sits (stable across turns for a prompt cache). It once rode `extra`,
        // which the seam clears, so a foreign model answered ungrounded. The Cohere writer lifts text
        // documents back into `documents` (COH-18). A non-array `documents` stays in `extra`.
        if let Some(docs) = obj.get(keys::DOCUMENTS).and_then(|d| d.as_array()) {
            let blocks: Vec<crate::codec::ir::IrBlock> =
                docs.iter().map(read_cohere_document).collect();
            match messages
                .iter_mut()
                .find(|m| m.role == crate::codec::ir::IrRole::User)
            {
                Some(first_user) => {
                    first_user.content.splice(0..0, blocks);
                }
                None if !blocks.is_empty() => messages.insert(
                    0,
                    crate::codec::ir::IrMessage {
                        role: crate::codec::ir::IrRole::User,
                        content: blocks,
                    },
                ),
                None => {}
            }
            extra.remove(keys::DOCUMENTS);
        }

        let reasoning = read_cohere_reasoning(obj.get(keys::THINKING));
        if reasoning.is_some() {
            // Promoted: the writer re-emits it from the typed field, so it must not also ride extra.
            extra.remove(keys::THINKING);
        }

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            // Request `logprobs` (bool) promoted so the ask carries cross-protocol.
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
        host: &dyn busbar_contract::codec::CodecHost,
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
                    // The response `id` rides the top-level message-start frame: captured for
                    // same-protocol passthrough, else synthesized shape-valid. Cohere has no stream `created`.
                    let id = data
                        .get(keys::ID)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .or_else(|| Some(synthesize_cohere_id(host)));
                    out.push(IrStreamEvent::MessageStart {
                        role: crate::codec::ir::IrRole::Assistant,
                        usage: None,
                        id,
                        created: None,
                        // The model when the frame carries one (compatible backends), so a cross-protocol
                        // ingress can surface it; native Cohere omits it, leaving None.
                        model: data
                            .get(keys::MODEL)
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                    });
                }
            }
            // Guard on `!text_block_closed`: once the text block has closed (content-end, or a leading
            // tool-plan's tool-call-start) a later text frame must not reopen it. A failed guard falls to
            // the `other` no-op arm, keeping egress balanced (no second content_block_start).
            // A `thinking` content block (a reasoning model streams it ahead of the answer) is its own
            // block with its own IR index (`THINKING_FRAME_BASE`); it never touches the text slot, so its
            // `content-end` cannot latch the text block closed before the answer starts (COH-01).
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
                    if let Some(index) = cohere_open_thinking_block(state, &mut out) {
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
                    }
                }
            }
            // A `thinking` content-delta (real Cohere sends `{ "thinking": "<chunk>" }` with no `type`)
            // streams into the open thinking block, opening one if `content-start` was skipped, never
            // into the text block (COH-02).
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
                        cohere_open_thinking_block(state, &mut out)
                    }
                };
                if let Some(index) = index.filter(|_| !text.is_empty()) {
                    out.push(IrStreamEvent::BlockDelta {
                        index,
                        delta: crate::codec::ir::IrDelta::ThinkingDelta(text.to_string()),
                    });
                }
            }
            ET_CONTENT_START if !state.text_block_closed => {
                // A text block opening while thinking is still open (upstream skipped `content-end`)
                // closes the thinking block first, keeping egress balanced.
                cohere_close_thinking_block(state, &mut out);
                // The text block claims a dynamic IR index by first appearance (`cohere_text_ir_index`),
                // not a hardcoded 0: a `tool-call-start` before any content frame already took 0, and
                // forcing 0 gave two BlockStarts at one index. `cohere_text_ir_index` also records
                // TEXT_BLOCK_SEEN_SENTINEL so a later tool stays off the text index even after content-end.
                // The raw wire `index` is never forwarded into the IR.
                if !state.text_block_open {
                    state.text_block_open = true;
                    let ti = cohere_text_ir_index(state);
                    out.push(IrStreamEvent::BlockStart {
                        index: ti,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
            }
            ET_CONTENT_DELTA if !state.text_block_closed => {
                cohere_close_thinking_block(state, &mut out);
                // The text block claims a dynamic IR index by first appearance (see content-start), not a
                // hardcoded 0, so a tool that opened first (and took 0) does not collide. The raw wire
                // `index` is never forwarded; every BlockStart/Delta below uses `text_idx`.
                let text_idx = cohere_text_ir_index(state);
                if !state.text_block_open {
                    state.text_block_open = true;
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
                            // Content-delta shape: real Cohere streams `{ "text": "<chunk>" }` with no
                            // `type` (only content-start has one); this file's writer emits
                            // `{ "type": "text", "text": … }`. Accept both: requiring `type == "text"` dropped
                            // every chunk from a real backend. Reject only an object declaring another type.
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
                // The chunk's logprobs ride the content-delta frame (`logprobs: {text, token_ids,
                // logprobs}`), mapped as the buffered `logprobs[]` items onto the text block (COH-14).
                if let Some(lp) = data.get(keys::LOGPROBS).and_then(read_cohere_logprob) {
                    out.push(IrStreamEvent::BlockDelta {
                        index: text_idx,
                        delta: crate::codec::ir::IrDelta::LogprobsDelta(vec![lp]),
                    });
                }
            }
            // Cohere v2 streams the pre-tool-call reasoning as `tool-plan-delta` frames (one token each at
            // `delta.message.tool_plan`) before `tool-call-start`, the streamed counterpart of the
            // non-stream `message.tool_plan` fold; without it a streaming hop drops the reasoning the
            // buffered one keeps. The plan opens a Thinking block, as the non-stream reader does, so a
            // stream and non-stream of one turn agree that the plan is not user-visible content. It claims
            // its index via `cohere_text_ir_index` (0 by first appearance, tools offsetting to 1+): v2
            // puts `tool_plan` and `content` in mutually exclusive turns, so the one pre-tool slot serves both.
            ET_TOOL_PLAN_DELTA if !state.text_block_closed => {
                cohere_close_thinking_block(state, &mut out);
                let plan_idx = cohere_text_ir_index(state);
                if !state.text_block_open {
                    state.text_block_open = true;
                    state.thinking_block_open = true;
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
                // content-end closes the text block at the IR index it claimed on first appearance
                // (`state.text_index`), not a hardcoded 0 (a tool may have taken 0). The raw wire
                // `index` is never forwarded. The stop is emitted only if a text block is open, so a stray
                // content-end cannot unbalance the stream. `state.text_index` is not cleared (like the
                // `open_tools` entries) so a tool opened after content-end still derives its base off
                // TEXT_BLOCK_SEEN_SENTINEL and cannot collide. `text_block_closed` latches here so a stray
                // text frame is dropped rather than reopening the stopped index.
                //
                // A content-end closing a thinking block closes only it: the text block is still to come,
                // so the text latch must not fire (COH-01).
                if cohere_open_thinking_index(state).is_some() {
                    cohere_close_thinking_block(state, &mut out);
                } else {
                    cohere_close_text_slot(state, &mut out);
                }
            }
            ET_MESSAGE_END => {
                // A stream ending with the leading tool-plan / text block still open (no content-end or
                // tool-call-start closed it: truncated or adversarial upstream) would leave a dangling
                // `content_block_start` on an Anthropic egress (unbalanced, a proxy tell). Force-close it
                // here before the terminal frames, at its claimed index; an open thinking block likewise.
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

                // The generic infra `ERROR` is a mid-stream upstream failure, but it only produced
                // `IrStopReason::Error` on the terminal MessageDelta, which no cross-protocol writer has a
                // native token for, so each renders a success terminator (`stop` / `end_turn`). With no
                // `IrStreamEvent::Error`, `terminal_error()` stays None, the breaker records no fault and
                // the failed stream bills as completed. Push the Error event too (as the gemini/bedrock
                // inline-error arms do) so the breaker sees it and the client gets a real error frame. The
                // MessageDelta/MessageStop still follow, so the stream is terminated and usage folded.
                //
                // Generic `ERROR` only: `ERROR_TOXIC` maps to `Safety`, a successfully served moderation
                // refusal that must not trip the breaker.
                if stop_reason == Some(crate::codec::ir::IrStopReason::Error) {
                    out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                        // The frame gives only the `ERROR` token, so classify it a transient server fault:
                        // the lane recovers via cooldown, not a permanent penalty.
                        class: busbar_contract::upstream::StatusClass::ServerError,
                        provider_signal: Some(raw_finish_reason.to_string()),
                        retry_after: None,
                    }));
                }

                // Billed counts: absent is zero, a present-but-unreadable count refuses and the stream
                // ends in an error instead of ledgering zero tokens.
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
                    // Cohere has no stop_sequence analog in its stream.
                    stop_sequence: None,
                    usage,
                    stop_detail: None,
                });
                out.push(IrStreamEvent::MessageStop);
            }
            // A tool call streams as tool-call-start / tool-call-delta(s) / tool-call-end under
            // `delta.message.tool_calls`, mapped onto BlockStart{ToolUse} / BlockDelta{InputJsonDelta} /
            // BlockStop as the OpenAI and Gemini readers do (otherwise streaming tool use is discarded).
            // Tool blocks occupy IR indices after any open text block.
            //
            // The IR index must be stable for a tool's whole lifetime. Cohere closes each tool before
            // opening the next, so deriving the index from the live rank of `frame_idx` was unstable: from a
            // set that shrank on end it collapsed later tools onto the first's index, and even from a
            // never-shrunk set a non-monotonic upstream `frame_idx` shifted an earlier tool's rank between
            // its start and end. So the index is assigned once at tool-call-start by insertion order
            // (`cohere_assign_tool_ir_index`), recorded in `state.tool_ir_index` keyed by frame index
            // (membership also in `state.open_tools`), and looked up verbatim on delta/end via an O(log n)
            // `BTreeMap`. Neither map is ever shrunk, so start/delta/end resolve to one IR index
            // whatever the wire-index order.
            ET_TOOL_CALL_START => {
                // Close a still-open text block before opening the tool. Real Cohere emits `content-end`
                // before the first `tool-call-start`, so on text-then-tool this is a no-op. But the
                // `tool-plan-delta` path opens a leading text block Cohere never closes with
                // `content-end` (it goes from plan tokens to `tool-call-start`, and `message-end` does not
                // close it), which would leave a `content_block_start` with no stop on an Anthropic egress
                // (unbalanced, a proxy tell). Close it at the index it claimed (`state.text_index`, not a
                // hardcoded 0) and clear the live flag. `state.text_index` stays recorded (as in
                // `content-end`) so TEXT_BLOCK_SEEN_SENTINEL still offsets this and later tools past the
                // text index. A thinking block left open ahead of tool calls is closed the same way.
                cohere_close_thinking_block(state, &mut out);
                cohere_close_text_slot(state, &mut out);
                let frame_idx = clamp_frame_index(data);
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
                // Assign and record the tool's immutable IR index. `None` for a duplicate start (re-emitting
                // BlockStart would add a spurious opening frame) or a new frame past the cap (untracked, its
                // delta/end would be dropped, so a BlockStart would orphan it). Emit only when freshly tracked.
                if let Some(ir_idx) = cohere_assign_tool_ir_index(state, frame_idx) {
                    out.push(IrStreamEvent::BlockStart {
                        index: ir_idx,
                        block: crate::codec::ir::IrBlockMeta::ToolUse { id, name },
                        refusal: false,
                    });
                    // Cohere may include initial argument text on the start frame.
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
                }
            }
            ET_TOOL_CALL_DELTA => {
                let frame_idx = clamp_frame_index(data);
                // Forward deltas only for a tracked frame (one we opened a BlockStart for), via its
                // assigned IR index. A frame past MAX_TRACKED_TOOL_FRAMES was never recorded, so its delta is
                // dropped rather than corrupting another block's arguments. Mirrors tool-call-end.
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
                let frame_idx = clamp_frame_index(data);
                // Close only a tool we opened, via its assigned IR index. The frame's entry is not
                // removed from `open_tools`/`tool_ir_index`: it keeps each tool's IR index stable for the
                // stream, and removal would let a later tool reuse a freed slot.
                if let Some(ir_idx) = state.tool_ir_index.get(&frame_idx).copied() {
                    out.push(IrStreamEvent::BlockStop { index: ir_idx });
                }
            }
            // Cohere v2's streamed grounding citation (docs.cohere.com/v2/docs/streaming): one Citation
            // at `delta.message.citations` while the answer's text block is open. It maps to
            // `IrDelta::CitationsDelta` on the text block's claimed IR index (the block the non-stream
            // reader attaches `message.citations` to), through `read_cohere_citations`, so a streamed and
            // buffered read of one turn carry the same citations. The arm was missing, so every streamed
            // citation vanished. An array body (the shape this crate's writer emitted before it was
            // made spec-conformant) is accepted too. A citation with no open text block (none yet, a
            // thinking/tool-plan block, or after content-end) is dropped: a delta into an unopened or
            // stopped index would unbalance every egress.
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
            // The structural close paired with each `citation-start`; no content. Cohere's own writer re-emits it from the citation delta.
            ET_CITATION_END => {}
            // Genuinely unknown event types are ignored: the stream may add frames (e.g. debug) with
            // no IR content; the `ET_*` arms above catch every tool-call frame. An empty type is folded
            // in on purpose: the discriminant is upstream-controlled `data["type"]`, so a missing field
            // is hostile input the reader must survive. A `debug_assert!(!other.is_empty())` here once
            // aborted debug/test builds on such a frame, violating the never-panic contract.
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
        host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;
        let message_val = obj.get(keys::MESSAGE).ok_or_else(ir_parse_error)?;

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        // Cohere v2's internal pre-tool-call plan is in `message.tool_plan` (a string, distinct from
        // `content[]`). It lands in the IR reasoning carrier (`Thinking`), not Text: as a leading Text it
        // was content injection, since every cross-protocol hop showed the model's internal plan to
        // the user as the answer's first paragraph and no writer could tell it from a real message.
        // `Thinking` fixes both: the plan stops being visible content, and the Cohere writer reshapes
        // it back into `tool_plan`. Writers with no reasoning slot drop it, correctly.
        if let Some(plan) = message_val.get(TOOL_PLAN).and_then(|p| p.as_str()) {
            if !plan.is_empty() {
                content.push(crate::codec::ir::IrBlock::Thinking {
                    text: plan.to_string(),
                    signature: None,
                    redacted: false,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                });
            }
        }
        // Grounding citations. Cohere's offsets are character offsets into the assembled content text,
        // so they belong on the first text block, where every writer's join starts (see the
        // base-offset accumulation in `openai_chat::url_annotations`); on each block they would multiply.
        let response_citations = message_val
            .get(keys::CITATIONS)
            .map(super::read_cohere_citations)
            .unwrap_or_default();
        let mut citations_pending = response_citations;
        if let Some(content_arr) = message_val.get(keys::CONTENT).and_then(|c| c.as_array()) {
            for block_val in content_arr {
                if let Some(block_obj) = block_val.as_object() {
                    match block_obj.get(keys::TYPE).and_then(|t| t.as_str()) {
                        Some(keys::TEXT) => {
                            if let Some(text) = block_obj.get(keys::TEXT).and_then(|t| t.as_str()) {
                                content.push(crate::codec::ir::IrBlock::Text {
                                    text: text.to_string(),
                                    cache_control: None,
                                    citations: std::mem::take(&mut citations_pending),
                                    refusal: false,
                                });
                            }
                        }
                        // A reasoning model's `{"type":"thinking","thinking":"…"}` part is the IR Thinking
                        // block, never visible text (it once vanished, COH-03).
                        Some(keys::THINKING) => {
                            if let Some(text) = block_obj
                                .get(keys::THINKING)
                                .and_then(|t| t.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                content.push(crate::codec::ir::IrBlock::Thinking {
                                    text: text.to_string(),
                                    signature: None,
                                    redacted: false,
                                    cache_control: None,
                                    kind: None,
                                    signature_origin: None,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Citations with no text block to ride (a grounded turn with no text part) were once lost with
        // the pending list (COH-17). They ride an empty text block after the content, ahead of the tool calls.
        if !citations_pending.is_empty() {
            content.push(crate::codec::ir::IrBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: std::mem::take(&mut citations_pending),
                refusal: false,
            });
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

        let raw_finish_reason = obj
            .get(keys::FINISH_REASON)
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let stop_reason = if raw_finish_reason.is_empty() {
            None
        } else {
            Some(read_cohere_stop_reason(raw_finish_reason))
        };

        // An absent `usage` is zero-defaulted, not an error: an upstream quirk (mock/staging/proxy
        // backend), and a `ClientError` would mislabel the cause and break retry logic. The Bedrock and
        // Gemini readers tolerate the same. Each token lookup below already defaults to 0.
        let usage_val = obj.get(keys::USAGE);
        // `cached_tokens` (the prompt-cache hit, counted inside `tokens.input_tokens`) stays in the
        // input count. `billed_units.search_units` is a separately metered unit, not a token count (see
        // `USAGE`). Absent is zero, a present-but-unreadable count refuses.
        let usage = read_cohere_usage(usage_val)?;

        // Response `logprobs[]`: one item per decoded text chunk, mapped to neutral per-span entries
        // (`read_cohere_logprob`); once dropped with a warn on every cross-protocol hop (COH-13).
        let logprobs: Vec<crate::codec::ir::IrTokenLogprob> = obj
            .get(keys::LOGPROBS)
            .and_then(|l| l.as_array())
            .map(|items| items.iter().filter_map(read_cohere_logprob).collect())
            .unwrap_or_default();

        let model = obj
            .get(keys::MODEL)
            .and_then(|m| m.as_str())
            .map(String::from);

        // Capture the response identity so Cohere to Cohere passthrough preserves it. v2 responses carry
        // an opaque UUID-like `id`; if omitted, a shape-valid one is synthesized so a native SDK always
        // sees a string. v2 has no `created`, `system_fingerprint` or `stop_sequence`: those stay `None`.
        let id = obj
            .get(keys::ID)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| Some(synthesize_cohere_id(host)));

        Ok(crate::codec::ir::IrResponse {
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
/// provider-metered bucket (token counts, `search_units`, `classifications`). `cached_tokens`, the
/// cache hit, is a slice inside `tokens.input_tokens` and is not split out: the cached share bills
/// at the input tier (only `tokens.*` is read and `cache_read_input_tokens` is `None`); moving it to
/// the cache-read tier would change what a key is charged.
///
/// Ledgering (the census over the pinned wire lock `testing/llm-conformance/wire/cohere.wire.json`
/// is `usage_census_tests`): raw token counts are the reserved input/output classes, billed token
/// counts ride beside them as attribution only (`IrUsage::to_token_usage`), and `search_units` and
/// `classifications` are open classes.
const USAGE: &[UsageCount] = &[
    (
        CountSlot::Input,
        CountRead::Zero(&[keys::TOKENS, keys::INPUT_TOKENS]),
    ),
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
