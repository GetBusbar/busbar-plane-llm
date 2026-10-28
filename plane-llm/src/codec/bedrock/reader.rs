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
    stream_keyed_by_event: true,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for BedrockReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        // The turn's `usage`, not a guardrail's. A guardrail assessment carries its own
        // `invocationMetrics.usage` (policy units) under a `trace` that can follow the turn's
        // `usage`; taking the last `"usage"` in the tail would read a zero-token turn. Walk back to
        // the last `usage` that names a token count; none is no recovery (the caller's floor),
        // never zero. The policy units are then read off that trailing `trace`, as buffered.
        const KEY: &[u8] = b"\"usage\"";
        let mut end = tail.len();
        while let Some(at) = tail[..end].windows(KEY.len()).rposition(|w| w == KEY) {
            if let Some(v) =
                super::super::usage_tail::isolate_tail_usage_object(&tail[at..end], KEY)
            {
                if TOKEN_USAGE_MEMBERS.iter().any(|k| v.get(*k).is_some()) {
                    // An unreadable billed count is refused on its own and bills nothing; the
                    // readable counts recover. The per-TTL cache-write split rides the same
                    // `usage` object, so an oversized body reports the breakdown a small one does.
                    let mut usage = read_bedrock_usage(Some(&v)).ok()?;
                    // The guardrail policy units the buffered and streamed reads bill: the answer's
                    // `trace` follows the turn's `usage`, so it is in the tail too. Only a `trace`
                    // after the turn's `usage` is the answer's own; one before it lies in the output.
                    if let Some(trace) = super::super::usage_tail::isolate_tail_usage_object(
                        &tail[at..],
                        b"\"trace\"",
                    ) {
                        let mut holder = serde_json::Map::new();
                        holder.insert(TRACE.to_string(), trace);
                        usage.detail.open_units =
                            guardrail_units(&serde_json::Value::Object(holder)).ok()?;
                    }
                    return Some(usage.to_token_usage());
                }
            }
            end = at;
        }
        None
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse the body once. Errors carry text in `message` and the type in `__type`
        // (`ValidationException`, `ThrottlingException`); the breaker's error_map keys on the type,
        // so it comes from `__type`.
        let (provider_code, structured_type) =
            match crate::codec::json::parse::<serde_json::Value>(body) {
                Ok(json) => {
                    let provider_code = json
                        .get(keys::MESSAGE)
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    // `__type` may carry a shape ARN suffix (`com.amazon...#ThrottlingException`);
                    // keep only the trailing token.
                    let structured_type = json
                        .get(super::DUNDER_TYPE)
                        .and_then(|t| t.as_str())
                        .map(|t| t.rsplit(['#', '/']).next().unwrap_or(t).to_string());
                    (provider_code, structured_type)
                }
                Err(_) => (None, None),
            };

        // Bedrock has no context-length error code: an oversized request is a generic
        // `ValidationException` whose `message` carries the signal ("Input is longer than the
        // maximum number of tokens allowed"). Without surfacing `context_length_exceeded` the
        // breaker would route it as a ClientError and penalize the lane instead of failing over.
        // As in `AnthropicReader::extract_error`, scan the body for the phrasing and override
        // `provider_code`; keep in sync with `classify` below.
        //
        // Gate the scan on a 400: Bedrock emits an oversized-context error only as a 400, never a
        // 5xx. Ungated, a 5xx whose body echoes the phrasing would be misclassified as ContextLength
        // and trigger a no-penalty failover masking an unhealthy lane.
        let provider_code = if status == StatusCode::BAD_REQUEST {
            let lower = String::from_utf8_lossy(body).to_lowercase();
            if lower.contains("input is longer than the maximum number of tokens")
                || (lower.contains(keys::MAXIMUM_TOKENS) && lower.contains(keys::REQUESTED))
                || (lower.contains("exceeds the maximum")
                    && (lower.contains(keys::TOKEN) || lower.contains(keys::CONTEXT)))
            {
                Some(busbar_contract::protocol::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
            } else {
                provider_code
            }
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
        crate::codec::dialect_classify::bedrock(status, body)
    }

    fn read_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrRequest, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        // Every unmodeled top-level field goes into `extra` so a Bedrock-to-Bedrock passthrough
        // re-emits it (`write_request` merges `req.extra`); dropping `topP`, `topK`,
        // `stopSequences`, `additionalModelRequestFields`, `guardrailConfig`,
        // `additionalModelResponseFieldPaths`, `performanceConfig`, `promptVariables` and the like
        // would change model behaviour (guardrails off, sampling reset).
        // `stream` is the route-injected discriminant captured into `IrRequest.stream`; it is not
        // echoed via `extra` (a native body never carries it, a tell). Modeled keys are
        // re-serialised by `write_request` from the IR, so excluding them avoids a double-emit.
        // `inferenceConfig` is deliberately not modeled-out: only `maxTokens` and `temperature` are
        // typed, and the rest (`stopSequences`, `topP`, `topK`, `stopCriteria`, future fields) would
        // be dropped. The whole raw object is captured and `write_request` overlays the two typed
        // fields onto it; they are still parsed into the IR for cross-protocol egress.
        // The modeled top-level keys handled structurally (not swept into `extra`) are a sorted
        // `&'static` slice probed with `binary_search`: allocation-free and faster than the
        // `HashSet` it replaced on this ingress hot path. It MUST stay sorted.
        // `toolConfig` is likewise deliberately not modeled-out: only `tools` (into `ir.tools`) is
        // typed, and a forced `toolChoice: {any: {}}` would be stripped on a same-protocol
        // passthrough, letting the model skip the tool. The whole raw object is captured and
        // `write_request` overlays the typed `tools` array; `tools` is still parsed into the IR.
        let mut extra = serde_json::Map::new();
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        // Native `cachePoint` markers with their original array index, for the writer to re-emit at
        // the same position same-protocol (see `CACHE_POINTS_SENTINEL`).
        let mut system_cache_points: Vec<serde_json::Value> = Vec::new();
        let mut message_cache_points: Vec<serde_json::Value> = Vec::new();
        // Native `guardContent` markers, same shape as the cachePoint capture
        // (`GUARD_CONTENT_SENTINEL`).
        let mut system_guard_content: Vec<serde_json::Value> = Vec::new();
        let mut message_guard_content: Vec<serde_json::Value> = Vec::new();
        // Native `document` / `video` blocks, same shape (`DOC_VIDEO_SENTINEL`); they appear only
        // in a message `content` array, never `system`.
        let mut message_doc_video: Vec<serde_json::Value> = Vec::new();

        // The family that minted this conversation's reasoning signatures, from the model id the
        // ingress placed in the body (`None` when the id does not reveal it).
        let signature_origin_here =
            bedrock_signature_origin(obj.get("model").and_then(|m| m.as_str()));

        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        if let Some(system_arr) = obj.get(keys::SYSTEM).and_then(|s| s.as_array()) {
            for (idx, sys_val) in system_arr.iter().enumerate() {
                if let Some(text_val) = sys_val.get(keys::TEXT).and_then(|t| t.as_str()) {
                    system_blocks.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                } else if let Some(cache_point) = sys_val.get(super::CACHE_POINT) {
                    // No IR counterpart for a prompt-cache marker: stash it with its original index so
                    // a same-protocol passthrough keeps prompt caching enabled.
                    system_cache_points.push(serde_json::json!({
                        (keys::I): idx,
                        (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                    }));
                    // Also map the marker onto the preceding block's IR `cache_control`: the stash is
                    // dropped at the cross-protocol seam, so a Bedrock-to-Anthropic hop would lose
                    // the boundary. Additive: the writer suppresses inline `cache_control` while the
                    // stash is present, so the two never double-emit.
                    set_preceding_block_cache_control(&mut system_blocks);
                } else if let Some(guard_content) = sys_val.get(super::GUARD_CONTENT) {
                    // The guardrail qualifiers have no IR counterpart: stash the whole block with its
                    // original index (`GUARD_CONTENT_SENTINEL`) so a same-protocol passthrough keeps
                    // the marked span. `b` is the IR index of the modelled block below, which the
                    // writer matches to suppress its own emission (see the `document` arm).
                    let modelled = guard_content_block(guard_content);
                    system_guard_content.push(serde_json::json!({
                        (keys::I): idx,
                        (super::B): modelled.as_ref().map(|_| system_blocks.len()),
                        (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                    }));
                    // The guarded span is prompt content: model it too, so a cross-protocol hop (stash
                    // cleared) still sends the caller's text.
                    system_blocks.extend(modelled);
                }
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            // A present but wrong-typed `messages` is a structural violation: reject with a 400
            // rather than coerce to an empty conversation. Absent stays lenient.
            let msgs_arr = messages_val.as_array().ok_or_else(ir_parse_error)?;
            for (msg_idx, msg_val) in msgs_arr.iter().enumerate() {
                let role_str = msg_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");

                let role = match role_str {
                    keys::USER => crate::codec::ir::IrRole::User,
                    keys::ASSISTANT => crate::codec::ir::IrRole::Assistant,
                    _ => return Err(ir_parse_error()),
                };

                let mut msg_content: Vec<crate::codec::ir::IrBlock> = Vec::new();
                // Converse `content` is array-only; a present wrong-typed `content` would be dropped
                // into an empty turn by the lenient projection below: reject with a 400. Absent
                // stays lenient.
                if let Some(cv) = msg_val.get(keys::CONTENT) {
                    if !cv.is_array() {
                        return Err(ir_parse_error());
                    }
                }
                if let Some(content_arr) = msg_val.get(keys::CONTENT).and_then(|c| c.as_array()) {
                    for (block_idx, content_val) in content_arr.iter().enumerate() {
                        if let Some(text_val) = content_val.get(keys::TEXT).and_then(|t| t.as_str())
                        {
                            msg_content.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                        } else if let Some(tool_use) = content_val.get(super::TOOL_USE_CAMEL) {
                            // A `toolUse` block must carry a non-empty string `toolUseId`, the key a later
                            // `toolResult` pairs against; reject rather than invent one.
                            let tu_id = tool_use
                                .get(super::TOOL_USE_ID)
                                .and_then(|id| id.as_str())
                                .filter(|s| !s.is_empty())
                                .ok_or_else(ir_parse_error)?
                                .to_string();
                            let name = tool_use
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input = tool_use
                                .get(keys::INPUT)
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);

                            msg_content.push(crate::codec::ir::IrBlock::ToolUse {
                                id: tu_id,
                                name,
                                input,
                                cache_control: None,
                                // Bedrock's wire has no Gemini thoughtSignature concept.
                                thought_signature: None,
                            });
                        } else if let Some(tool_result) = content_val.get(super::TOOL_RESULT) {
                            let tu_id = tool_result
                                .get(super::TOOL_USE_ID)
                                .and_then(|id| id.as_str())
                                .unwrap_or("")
                                .to_string();

                            let mut inner_content: Vec<crate::codec::ir::IrBlock> = Vec::new();
                            if let Some(inner_arr) =
                                tool_result.get(keys::CONTENT).and_then(|c| c.as_array())
                            {
                                for inner_val in inner_arr {
                                    if let Some(text_val) =
                                        inner_val.get(keys::TEXT).and_then(|t| t.as_str())
                                    {
                                        inner_content.push(crate::codec::ir::IrBlock::text(
                                            text_val.to_string(),
                                        ));
                                    } else if let Some(json_val) = inner_val.get(keys::JSON) {
                                        // A `{"json": <value>}` tool-result block is structured data
                                        // with no text/image analog: carried as `IrBlock::Json` so
                                        // `write_request` keeps the json/text distinction.
                                        inner_content.push(crate::codec::ir::IrBlock::Json(
                                            json_val.clone(),
                                        ));
                                    } else if let Some(image) = inner_val.get(keys::IMAGE) {
                                        // `ToolResultContentBlock` also includes `image` (and
                                        // `document`/`video`). Decode `image` symmetric with the
                                        // writer, which emits one inside a toolResult.
                                        if let Some(block) = read_bedrock_image_block(image) {
                                            inner_content.push(block);
                                        }
                                    } else if let Some(document) = inner_val.get(keys::DOCUMENT) {
                                        // The `document` member is the same DocumentBlock as the
                                        // top-level one, so it models as the same IR `Media`.
                                        inner_content.push(bedrock_media_block(
                                            crate::codec::ir::IrMediaKind::Document,
                                            document,
                                        ));
                                    } else if let Some(video) = inner_val.get(super::VIDEO) {
                                        // Likewise the `video` member.
                                        inner_content.push(bedrock_media_block(
                                            crate::codec::ir::IrMediaKind::Video,
                                            video,
                                        ));
                                    } else if let Some(sr) =
                                        inner_val.get(super::SEARCH_RESULT_CAMEL)
                                    {
                                        // A `searchResult` a tool returned: the search-result slot.
                                        inner_content.push(super::read_bedrock_search_result(sr));
                                    }
                                }
                            }

                            let is_error = tool_result
                                .get(keys::STATUS)
                                .and_then(|s| s.as_str())
                                .map(|s| s == keys::ERROR_WORD)
                                .unwrap_or(false);

                            msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                                tool_use_id: tu_id,
                                content: inner_content,
                                is_error,
                                cache_control: None,
                            });
                        } else if let Some(image) = content_val.get(keys::IMAGE) {
                            // Decode both `source.bytes` (base64) and `source.s3Location` (an S3 URI).
                            // An S3 image rides `IrImageSource::Vendor { vendor: "bedrock", value:
                            // {format, s3Location} }` (see mod.rs) so the writer re-emits it
                            // same-protocol.
                            if let Some(block) = read_bedrock_image_block(image) {
                                msg_content.push(block);
                            }
                        } else if let Some(reasoning) = content_val.get(super::REASONING_CONTENT) {
                            // A `reasoningContent` block maps onto IR `Thinking { text, signature }`.
                            // Bedrock requires the signed reasoning echoed on the follow-up turn, so
                            // dropping it would diverge from a direct AWS call. `redactedContent`
                            // re-emits faithfully; a future union member yields `None` (undecoded, not
                            // mis-mapped). `redacted` is a typed flag set only on a genuine
                            // `redactedContent`, so a client cannot forge one via `signature`.
                            if let Some(mut block) = read_bedrock_reasoning_block(reasoning) {
                                // Who minted the signature is visible only through the model id.
                                if let crate::codec::ir::IrBlock::Thinking {
                                    signature: Some(_),
                                    signature_origin,
                                    ..
                                } = &mut block
                                {
                                    if signature_origin.is_none() {
                                        *signature_origin = signature_origin_here;
                                    }
                                }
                                msg_content.push(block);
                            }
                        } else if let Some(cc) = content_val.get(super::CITATIONS_CONTENT) {
                            // A cited assistant turn carries its answer text inside `citationsContent`.
                            msg_content.push(read_bedrock_citations_content(cc));
                        } else if let Some(cache_point) = content_val.get(super::CACHE_POINT) {
                            // No IR counterpart for a prompt-cache marker: stash it with its
                            // (message, block) index so a same-protocol passthrough keeps prompt
                            // caching enabled (dropping it is a cost regression).
                            message_cache_points.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                            }));
                            // Also map the marker onto the preceding block's `cache_control` so the
                            // boundary survives a cross-protocol hop. Additive (see
                            // `set_preceding_block_cache_control`); no double-emit.
                            set_preceding_block_cache_control(&mut msg_content);
                        } else if let Some(guard_content) = content_val.get(super::GUARD_CONTENT) {
                            // The guardrail qualifiers have no IR counterpart: stash the block with its
                            // (message, block) index (`GUARD_CONTENT_SENTINEL`). `b` is the IR index
                            // of the modelled block below (see the `document` arm).
                            let modelled = guard_content_block(guard_content);
                            message_guard_content.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): modelled.as_ref().map(|_| msg_content.len()),
                                (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                            }));
                            // The guarded text/image is prompt content; model it so a cross-protocol hop
                            // carries it (the writer suppresses it while the stash is present).
                            msg_content.extend(modelled);
                        } else if let Some(document) = content_val.get(keys::DOCUMENT) {
                            // A `document` block has no IR counterpart: stash it verbatim with its
                            // (message, block) index (`DOC_VIDEO_SENTINEL`) so a same-protocol
                            // passthrough keeps the attachment.
                            // Two index spaces: `i` is the wire position the raw block is spliced
                            // back at, `b` the modelled block's position in the IR content list,
                            // which the writer matches to suppress its own emission. They diverge
                            // once a block with a wire slot but no IR block (`cachePoint`,
                            // `guardContent`) precedes this one; matching the wire index against an
                            // IR index would then fail and the document would go upstream twice.
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (keys::DOCUMENT): document.clone() },
                            }));
                            // Also model it, the additive pattern `set_preceding_block_cache_control`
                            // uses: the stash returns only on a Bedrock-to-Bedrock hop (the seam
                            // clears `extra`), so cross-protocol the caller's PDF would otherwise
                            // vanish with no warn though Gemini and Anthropic have a slot for it. The
                            // writer suppresses this block while the stash is present, keeping the
                            // same-protocol round-trip byte-identical.
                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Document,
                                document,
                            ));
                        } else if let Some(video) = content_val.get(super::VIDEO) {
                            // A `video` block likewise has no IR counterpart: stash it verbatim
                            // (`DOC_VIDEO_SENTINEL`). `b` is the IR index, `i` the wire slot (see the
                            // `document` arm).
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (super::VIDEO): video.clone() },
                            }));
                            // Modelled for the cross-protocol hop as well — see the `document` arm.
                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Video,
                                video,
                            ));
                        } else if let Some(sr) = content_val.get(super::SEARCH_RESULT_CAMEL) {
                            // A caller's `searchResult` (RAG passage) is parked verbatim in the same
                            // splice store as `document` / `video` and modelled as the search-result
                            // slot cross-protocol (the writer suppresses the copy while the stash is
                            // present). See the `document` arm for `i` versus `b`.
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (super::SEARCH_RESULT_CAMEL): sr.clone() },
                            }));
                            msg_content.push(super::read_bedrock_search_result(sr));
                        }
                    }
                }

                messages.push(crate::codec::ir::IrMessage {
                    role,
                    content: msg_content,
                });
            }
        }

        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        if let Some(tool_config) = obj.get(keys::TOOL_CONFIG).and_then(|t| t.as_object()) {
            if let Some(tools_arr) = tool_config.get(keys::TOOLS).and_then(|t| t.as_array()) {
                for tool_val in tools_arr {
                    if let Some(tool_spec) =
                        tool_val.get(super::TOOL_SPEC).and_then(|t| t.as_object())
                    {
                        let name = tool_spec
                            .get(keys::NAME)
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let description = tool_spec
                            .get(keys::DESCRIPTION)
                            .and_then(|d| d.as_str().map(String::from));

                        let input_schema =
                            if let Some(input_schema) = tool_spec.get(super::INPUT_SCHEMA_CAMEL) {
                                input_schema
                                    .get(keys::JSON)
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null)
                            } else {
                                serde_json::Value::Null
                            };

                        tools.push(crate::codec::ir::IrTool {
                            name,
                            description,
                            input_schema,
                            cache_control: None,
                            hosted: None,
                            // `toolSpec.strict` (structured-output enforcement on the tool's input) is
                            // the same per-tool switch the IR carries.
                            strict: tool_spec.get(keys::STRICT).and_then(|v| v.as_bool()),
                        });
                    } else if tool_val.get(super::CACHE_POINT).is_some() {
                        // A `cachePoint` in `toolConfig.tools` marks the cache boundary for the tool
                        // definitions before it (Anthropic puts the same breakpoint on a tool). It
                        // maps onto the preceding tool's `cache_control`; with no positional stash this
                        // is the sole carrier, and the writer re-emits the marker from it. A leading
                        // cachePoint has nothing to attach to and is dropped.
                        //
                        // Accepted: a leading or doubled adjacent cachePoint does not byte-round-trip
                        // (doubled ones collapse onto the one tool's boolean). AWS itself rejects
                        // those shapes, so no valid request loses fidelity.
                        if let Some(last) = tools.last_mut() {
                            last.cache_control = Some(crate::codec::ir::CacheControl {
                                kind: crate::codec::ir::CacheKind::Ephemeral,
                            });
                        }
                    }
                }
            }
        }

        // Promote `toolConfig.toolChoice` into the IR union so a forced or targeted directive
        // survives the seam.
        let tool_choice = read_bedrock_tool_choice(obj.get(keys::TOOL_CONFIG));

        let max_tokens = if let Some(inference_config) =
            obj.get(super::INFERENCE_CONFIG).and_then(|i| i.as_object())
        {
            inference_config
                .get(super::MAX_TOKENS_CAMEL)
                .and_then(|v| v.as_u64())
                .filter(|&v| v > 0)
                // Bounds-checked: `as u32` would wrap a value above u32::MAX (5_000_000_000 becomes
                // 705_032_704) into a cap the caller never asked for. Out-of-range drops to None
                // so the backend applies its default.
                .and_then(|v| u32::try_from(v).ok())
        } else {
            None
        };

        // `inferenceConfig.{temperature, topP, stopSequences}` are mapping-file rows, read below,
        // and also preserved in the raw `inferenceConfig` in `extra`; the writer overlays the typed
        // fields onto it. `topK` is not an inferenceConfig field.
        // Promote `top_k`: Converse carries it only via the model-specific
        // `additionalModelRequestFields` escape hatch, spelled `top_k` or `topK`. Accept either so
        // it populates the IR field and survives the seam (where `extra` is cleared); the raw
        // object stays in `extra` and the writer overlays the typed `top_k` back.
        // `top_k_was_camel` is true only when the value came from `topK`, so a same-protocol
        // passthrough is not renamed to `top_k`.
        let amrf = obj
            .get(super::ADDITIONAL_MODEL_REQUEST_FIELDS)
            .and_then(|v| v.as_object());
        let mut top_k_was_camel = false;
        let top_k = amrf
            .and_then(|amrf| {
                amrf.get(super::TOP_K).or_else(|| {
                    top_k_was_camel = true;
                    amrf.get(super::TOP_K_CAMEL)
                })
            })
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        // Reset unless a usable top_k came from the camel key, so no other value stamps the sentinel.
        top_k_was_camel &= top_k.is_some();

        // Stash captured `cachePoint` markers under the sentinel, only when at least one was present,
        // so a cache-free body gains no stray key and round-trips byte-exact.
        if !system_cache_points.is_empty() || !message_cache_points.is_empty() {
            let mut cache_points = serde_json::Map::new();
            if !system_cache_points.is_empty() {
                cache_points.insert(
                    keys::SYSTEM.to_string(),
                    serde_json::Value::Array(system_cache_points),
                );
            }
            if !message_cache_points.is_empty() {
                cache_points.insert(
                    keys::MESSAGES.to_string(),
                    serde_json::Value::Array(message_cache_points),
                );
            }
            extra.insert(
                CACHE_POINTS_SENTINEL.to_string(),
                serde_json::Value::Object(cache_points),
            );
        }

        // Stash captured `guardContent` markers likewise, only when present.
        if !system_guard_content.is_empty() || !message_guard_content.is_empty() {
            let mut guard_content = serde_json::Map::new();
            if !system_guard_content.is_empty() {
                guard_content.insert(
                    keys::SYSTEM.to_string(),
                    serde_json::Value::Array(system_guard_content),
                );
            }
            if !message_guard_content.is_empty() {
                guard_content.insert(
                    keys::MESSAGES.to_string(),
                    serde_json::Value::Array(message_guard_content),
                );
            }
            extra.insert(
                GUARD_CONTENT_SENTINEL.to_string(),
                serde_json::Value::Object(guard_content),
            );
        }

        // Stash captured `document` / `video` markers likewise, only when present.
        if !message_doc_video.is_empty() {
            let mut doc_video = serde_json::Map::new();
            doc_video.insert(
                keys::MESSAGES.to_string(),
                serde_json::Value::Array(message_doc_video),
            );
            extra.insert(
                DOC_VIDEO_SENTINEL.to_string(),
                serde_json::Value::Object(doc_video),
            );
        }

        // Stamp the spelling hint when top_k arrived as `topK`, so the writer re-emits `topK`
        // same-protocol (else `top_k`; `extra` is cleared across protocols). Only when it produced a
        // usable value.
        if top_k_was_camel {
            extra.insert(
                TOP_K_CAMEL_SENTINEL.to_string(),
                serde_json::Value::Bool(true),
            );
        }

        // The reasoning asks in `additionalModelRequestFields` (Anthropic-on-Bedrock `thinking` and
        // `output_config.effort`, Nova `reasoningConfig`) are not promoted to the IR: carrying them
        // would switch on billed thinking on a foreign lane. The raw object stays in `extra`; the
        // one registered crossing, top-level `outputConfig.effort`, is the map file's row.
        let reasoning = None;
        // Converse's native structured output, `outputConfig.textFormat`.
        let response_format = read_bedrock_response_format(obj);

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            system: system_blocks,
            messages,
            tools,
            max_tokens,
            top_k,
            tool_choice,
            // A Converse body has no `stream` field; the endpoint selects streaming. The ingress
            // route therefore injects `"stream": true` for converse-stream requests (see
            // `ingress_path_model`), and the IR must carry it through or the egress writer is never
            // told to stream and the client silently gets a buffered response. False when absent.
            stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
            response_format,
            // `requestMetadata` crosses as the typed metadata (the raw object stays in `extra`).
            metadata: read_bedrock_request_metadata(obj),
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

        if !data.is_object() {
            return out;
        }

        match data.get(keys::TYPE).and_then(|t| t.as_str()) {
            Some(ET_MESSAGE_START) => {
                if !state.started {
                    state.started = true;
                    out.push(IrStreamEvent::MessageStart {
                        role: crate::codec::ir::IrRole::Assistant,
                        usage: None,
                        id: None,
                        created: None,
                        model: None,
                    });
                }
            }

            Some(ET_CONTENT_BLOCK_START) => {
                let idx = clamp_content_block_index(data);

                if let Some(start_obj) = data.get(keys::START).and_then(|s| s.as_object()) {
                    if let Some(tool_use) = start_obj
                        .get(super::TOOL_USE_CAMEL)
                        .and_then(|t| t.as_object())
                    {
                        // As the text branch's `state.started` guard: a BlockStart must never precede
                        // its MessageStart (a malformed or reordered stream would break the IR
                        // ordering invariant). Skip it.
                        if state.started {
                            let tu_id = tool_use
                                .get(super::TOOL_USE_ID)
                                .and_then(|id| id.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = tool_use
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();

                            // Record the opened index (as the Gemini/OpenAI/Cohere readers do with
                            // `open_tools`) so the matching `contentBlockStop` can verify a
                            // BlockStart happened before emitting a BlockStop.
                            state.open_tools.insert(idx);

                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block: crate::codec::ir::IrBlockMeta::ToolUse { id: tu_id, name },
                                refusal: false,
                            });
                        }
                    } else if start_obj.contains_key(super::REASONING_CONTENT)
                        && state.started
                        && !state.thinking_block_open
                    {
                        // Some compatible backends prefix a reasoning block with a `contentBlockStart`
                        // carrying an empty `reasoningContent` object. Open the Thinking block here;
                        // the delta arm's lazy-open then sees the flag set. (The native
                        // `ContentBlockStart` union models only `toolUse`, so real AWS never takes
                        // this branch.)
                        state.thinking_block_open = true;
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                            refusal: false,
                        });
                    } else if start_obj.is_empty() && state.started && !state.text_block_open {
                        // The native wire sends `contentBlockStart` with an empty `start: {}` for a text
                        // block; only that shape opens Text. A `start` with an unrecognized key (a
                        // future block type) is skipped rather than mis-opened, as in the Gemini and
                        // Cohere readers.
                        state.text_block_open = true;
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Text,
                            refusal: false,
                        });
                    }
                } else if state.started && !state.text_block_open {
                    // No `start` object at all: a text block.
                    state.text_block_open = true;
                    out.push(IrStreamEvent::BlockStart {
                        index: idx,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
            }

            Some(ET_CONTENT_BLOCK_DELTA) => {
                let idx = clamp_content_block_index(data);

                if let Some(delta_obj) = data.get(keys::DELTA).and_then(|d| d.as_object()) {
                    if delta_obj.contains_key(keys::TEXT) {
                        let text_val = delta_obj
                            .get(keys::TEXT)
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();

                        // Lazily open the Text block on the first text delta. Real AWS sends no
                        // `contentBlockStart` for text (`ContentBlockStart$start` models only
                        // `toolUse`); the first `contentBlockDelta` with `text` implies it, and
                        // without this the delta would be an orphan with no BlockStart. An explicit
                        // empty-`start` has already set the flag, so it is not re-opened.
                        if state.started && !state.text_block_open {
                            state.text_block_open = true;
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block: crate::codec::ir::IrBlockMeta::Text,
                                refusal: false,
                            });
                        }

                        // Emit the delta only once the text block is open; on a malformed stream (a
                        // delta before messageStart) the orphan is dropped, as in the
                        // reasoningContent arm below.
                        if state.text_block_open {
                            out.push(IrStreamEvent::BlockDelta {
                                index: idx,
                                delta: crate::codec::ir::IrDelta::TextDelta(text_val),
                            });
                        }
                    } else if let Some(tool_use) = delta_obj
                        .get(super::TOOL_USE_CAMEL)
                        .and_then(|t| t.as_object())
                    {
                        // A tool-use input delta is valid only inside a block the `contentBlockStart`
                        // arm opened (`open_tools`); one preceding its start is dropped.
                        if let Some(input_str) = tool_use.get(keys::INPUT).and_then(|i| i.as_str())
                        {
                            if state.started && state.open_tools.contains(&idx) {
                                out.push(IrStreamEvent::BlockDelta {
                                    index: idx,
                                    delta: crate::codec::ir::IrDelta::InputJsonDelta(
                                        input_str.to_string(),
                                    ),
                                });
                            }
                        }
                    } else if let Some(citation) =
                        delta_obj.get(keys::CITATION).filter(|c| c.is_object())
                    {
                        // A `citation` delta (one `CitationsDelta` per frame) arrives interleaved with
                        // the `text` deltas it cites, at the same `contentBlockIndex`. Open the text
                        // block if the citation came first, as the text arm does.
                        if state.started && !state.text_block_open {
                            state.text_block_open = true;
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block: crate::codec::ir::IrBlockMeta::Text,
                                refusal: false,
                            });
                        }
                        if state.text_block_open {
                            out.push(IrStreamEvent::BlockDelta {
                                index: idx,
                                delta: crate::codec::ir::IrDelta::CitationsDelta(vec![
                                    read_bedrock_citation(citation),
                                ]),
                            });
                        }
                    } else if let Some(reasoning) = delta_obj
                        .get(super::REASONING_CONTENT)
                        .and_then(|r| r.as_object())
                    {
                        // Extended thinking streams as a `reasoningContent` member on
                        // `contentBlockDelta`, mirroring `read_bedrock_reasoning_block`. The wire sends
                        // no `contentBlockStart` for it, so the Thinking block opens lazily on the
                        // first delta, then the matching delta is emitted.
                        //
                        // The `ReasoningContentBlockDelta` union has three members:
                        //   - `text`            → ThinkingDelta(text)
                        //   - `signature`       → SignatureDelta(signature)  (the opaque token)
                        //   - `redactedContent` → RedactedReasoningDelta(bytes)  (opaque encrypted
                        //                         reasoning). A typed delta distinct from the
                        //                         plaintext one keeps the block to one IR delta and
                        //                         one frame, so the writer re-emits it with no
                        //                         `text` leak; non-Bedrock writers drop it.
                        if state.started && !state.thinking_block_open {
                            state.thinking_block_open = true;
                            // A redacted block's only member is `redactedContent`, so peek it at open
                            // time to open with `RedactedThinking` rather than `Thinking`, letting a
                            // cross-protocol writer (e.g. Anthropic) emit `redacted_thinking`, not a
                            // plaintext seed. The bytes ride the `RedactedReasoningDelta` below.
                            let block = if reasoning.contains_key(super::REDACTED_CONTENT) {
                                crate::codec::ir::IrBlockMeta::RedactedThinking
                            } else {
                                crate::codec::ir::IrBlockMeta::Thinking { kind: None }
                            };
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block,
                                refusal: false,
                            });
                        }
                        if state.thinking_block_open {
                            if let Some(text) = reasoning.get(keys::TEXT).and_then(|t| t.as_str()) {
                                out.push(IrStreamEvent::BlockDelta {
                                    index: idx,
                                    delta: crate::codec::ir::IrDelta::ThinkingDelta(
                                        text.to_string(),
                                    ),
                                });
                            } else if let Some(sig) =
                                reasoning.get(keys::SIGNATURE).and_then(|s| s.as_str())
                            {
                                out.push(IrStreamEvent::BlockDelta {
                                    index: idx,
                                    delta: crate::codec::ir::IrDelta::SignatureDelta(
                                        sig.to_string(),
                                    ),
                                });
                            } else if let Some(redacted) = reasoning
                                .get(super::REDACTED_CONTENT)
                                .and_then(|r| r.as_str())
                            {
                                out.push(IrStreamEvent::BlockDelta {
                                    index: idx,
                                    delta: crate::codec::ir::IrDelta::RedactedReasoningDelta(
                                        redacted.to_string(),
                                    ),
                                });
                            }
                            // A future member with none of the three keys carries no IR delta; the block
                            // stays open and it is skipped, as the buffered reader's `None` arm.
                        }
                    }
                }
            }

            Some(ET_CONTENT_BLOCK_STOP) => {
                let idx = clamp_content_block_index(data);

                // As the START arms: a BlockStop must never precede its MessageStart nor be emitted
                // unless a block is known open. A malformed, reordered or duplicate
                // `contentBlockStop` would produce a `BlockStop` with no matching `BlockStart`,
                // breaking the stream-balance invariant and serializing an invalid
                // `content_block_stop` cross-protocol (e.g. Anthropic). Such a frame is skipped
                // silently rather than erroring the stream.
                if state.started {
                    // Check `open_tools` first. Tool blocks are tracked by index, which is more precise
                    // than the index-blind text/thinking flags (valid only if at most one block of any
                    // kind is open, true on a well-formed wire but not a malformed one). A STOP
                    // matching a tracked tool index is then never misattributed to a flag, no stale
                    // entry swallows a later text/thinking STOP, and concurrently open tool blocks
                    // close independently.
                    if state.open_tools.remove(&idx) {
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.text_block_open {
                        // Clear `text_block_open` on any contentBlockStop while a text block is open, not
                        // only at index 0: text after a tool-use block sits at index > 0, and an
                        // index-0 guard would leave the flag set and suppress every later text block.
                        // At most one text block is open at a time on this wire, so the flag belongs
                        // to the block being stopped.
                        state.text_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.thinking_block_open {
                        // Clear the reasoning flag on its stop too, so a later reasoning or text block
                        // opens cleanly.
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    }
                    // else: no block is known open for `idx` (an unmatched or duplicate stop); skip it.
                }
            }

            Some(ET_MESSAGE_STOP) => {
                // Bedrock splits the stop reason (`messageStop`) from the usage (a following
                // `metadata`). To give a cross-protocol ingress (e.g. Anthropic) the single
                // `message_delta` a native stream carries, the stop_reason is buffered here and paired
                // with the usage in the `metadata` branch, which emits the combined
                // `MessageDelta{stop_reason, usage}`.
                //
                // The terminal `MessageStop` is also deferred to that branch, after the combined
                // delta: emitted here it would precede `metadata`, and an Anthropic ingress would
                // write `message_stop` before `message_delta`. A Bedrock-to-Bedrock round-trip is
                // unaffected: `MessageStop` maps to no wire frame (`BedrockWriter` returns `None`) and
                // `StreamTranslate` re-splits the combined delta into `messageStop` + `metadata`.
                let stop_reason = data.get(super::STOP_REASON).and_then(|s| s.as_str());
                state.pending_stop_reason = stop_reason.map(stop_reason_map);
                // The refinement and the matched stop string ride this frame too, buffered with the
                // reason for the combined delta.
                state.pending_stop_detail = stop_reason.and_then(stop_detail_map);
                state.pending_stop_sequence = data
                    .get(super::ADDITIONAL_MODEL_RESPONSE_FIELDS)
                    .and_then(|f| f.get(keys::STOP_SEQUENCE))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }

            Some(ET_METADATA) => {
                // Usage trails the stop reason. Pair it with the buffered stop_reason into one combined
                // MessageDelta, so a cross-protocol ingress emits a single `message_delta`; a
                // Bedrock-to-Bedrock round-trip re-splits it in the writer fan-out driven by
                // `StreamTranslate`.
                //
                // The terminal `MessageStop` is emitted here after the combined delta (delta-then-stop,
                // the native order), unconditionally, so the stream always gets its terminal frame.
                // The combined MessageDelta is also emitted unconditionally, even when `metadata`
                // carries no `usage` (a mock or compatible backend may omit it). Taking
                // `pending_stop_reason` only inside the `usage` guard dropped the stop_reason and ended
                // the stream with a bare MessageStop, an ordering violation for Anthropic and a lost
                // stop. Usage is built from whatever the frame carries (zero when absent), the delta
                // consumes the buffered stop_reason, and a bare `metadata` with neither yields a benign
                // zero-usage delta.
                // Billed counts: absent is zero, a present but unreadable count refuses (the stream
                // ends in an error) instead of ledgering zero, for the cache counts and per-TTL split
                // as much as input/output; an unreadable cache count must not read as "no cache".
                //
                // The per-TTL cache-write split rides the stream's `metadata` frame as it rides the
                // buffered `usage`: the TTLs price differently, so the bill must reconcile both ways.
                let usage_val = data.get(keys::USAGE);
                let mut usage = match read_bedrock_usage(usage_val) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };
                // The guardrail policy units ride the same frame's `trace`, as buffered, and travel on
                // the usage under their open classes; an unreadable one refuses.
                usage.detail.open_units = match guardrail_units(data) {
                    Ok(units) => units,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };

                if let Some(tier) = read_served_tier(data) {
                    usage.detail.service_tier = Some(tier);
                }
                out.push(IrStreamEvent::MessageDelta {
                    stop_reason: state.pending_stop_reason.take(),
                    stop_sequence: state.pending_stop_sequence.take(),
                    usage,
                    stop_detail: state.pending_stop_detail.take(),
                });
                out.push(IrStreamEvent::MessageStop);
            }

            // Mid-stream exception events. The `responseStream` union has exactly five error members
            // (`internalServerException`, `modelStreamErrorException`, `validationException`,
            // `throttlingException`, `serviceUnavailableException`), any of which can arrive in
            // place of or before `messageStop`. `modelTimeoutException` is a request-level exception,
            // never emitted mid-stream, so it is not accepted (see `bedrock_stream_exception_for`).
            // A recognized event surfaces as `IrStreamEvent::Error` so the ingress writer ends the
            // stream with a protocol-shaped error, not a hang or an EOF without a terminator.
            Some(
                exc @ (super::INTERNAL_SERVER_EXCEPTION
                | super::MODEL_STREAM_ERROR_EXCEPTION
                | super::THROTTLING_EXCEPTION
                | super::VALIDATION_EXCEPTION
                | super::SERVICE_UNAVAILABLE_EXCEPTION),
            ) => {
                let message = data
                    .get(keys::MESSAGE)
                    .and_then(|m| m.as_str())
                    .map(String::from);
                // Map each of the five exception strings to its StatusClass, listed explicitly so the
                // mapping sits beside the string set rather than behind a `_ => ServerError` default.
                // The `other =>` arm is kept only because `&str` matches are never exhaustive; the
                // outer pattern is the real guard.
                let class = match exc {
                    super::THROTTLING_EXCEPTION => StatusClass::RateLimit,
                    super::VALIDATION_EXCEPTION => StatusClass::ClientError,
                    super::SERVICE_UNAVAILABLE_EXCEPTION => StatusClass::Overloaded,
                    super::INTERNAL_SERVER_EXCEPTION | super::MODEL_STREAM_ERROR_EXCEPTION => {
                        StatusClass::ServerError
                    }
                    // Unreachable: the outer guard restricts `exc` to the five strings above. A named
                    // binding (as in `bearer_error_code`) keeps the arm explicit; ServerError is the
                    // safe class for an unknown exception.
                    other => {
                        let _ = other;
                        StatusClass::ServerError
                    }
                };
                out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                    class,
                    provider_signal: message.or_else(|| Some(exc.to_string())),
                    retry_after: None,
                }));
            }

            // Any other or absent event type is a no-op: this is the demux for an open-ended event
            // stream, so an unrecognized future event is skipped, not an error. Error-bearing types
            // are handled above.
            Some(_) | None => {}
        }

        out
    }

    fn read_response(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        body: &serde_json::Value,
    ) -> Result<crate::codec::ir::IrResponse, IrError> {
        let obj = body.as_object().ok_or_else(ir_parse_error)?;

        // Documented cross-protocol drop (warned). A Converse response can carry Bedrock-only
        // members the IR has no home for: `trace` (`guardrail` detail, `promptRouter` metadata),
        // `additionalModelResponseFields` and the response-side `performanceConfig`. A
        // Bedrock-to-Bedrock hop keeps them byte-verbatim via the proxy short-circuit (this reader
        // is not called); this reader runs only cross-protocol, where they legitimately drop, so
        // warn once per present field. (`IrResponse` has no `extra` carrier by design.)
        for dropped in [
            super::TRACE,
            super::ADDITIONAL_MODEL_RESPONSE_FIELDS,
            "performanceConfig",
        ] {
            if obj.contains_key(dropped) {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::wire(dropped),
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [field = dropped,],
                    "dropping Bedrock-only Converse response member `{dropped}` on a cross-protocol \
                     egress: it has no neutral-IR carrier and no equivalent in any other protocol, \
                     so it cannot be projected to a non-Bedrock client (a same-protocol \
                     Bedrock->Bedrock hop preserves it byte-verbatim via the proxy short-circuit)"
                );
            }
        }

        let output_val = obj.get(keys::OUTPUT).ok_or_else(ir_parse_error)?;

        let message_val = output_val.get(keys::MESSAGE).ok_or_else(ir_parse_error)?;

        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();

        if let Some(content_arr) = message_val.get(keys::CONTENT).and_then(|c| c.as_array()) {
            for block_val in content_arr {
                if let Some(text_val) = block_val.get(keys::TEXT).and_then(|t| t.as_str()) {
                    content.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                } else if let Some(tool_use) = block_val
                    .get(super::TOOL_USE_CAMEL)
                    .and_then(|t| t.as_object())
                {
                    let tu_id = tool_use
                        .get(super::TOOL_USE_ID)
                        .and_then(|id| id.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tool_use
                        .get(keys::NAME)
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = tool_use
                        .get(keys::INPUT)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);

                    content.push(crate::codec::ir::IrBlock::ToolUse {
                        id: tu_id,
                        name,
                        input,
                        cache_control: None,
                        thought_signature: None,
                    });
                } else if let Some(cc) = block_val.get(super::CITATIONS_CONTENT) {
                    // A cited answer carries its text inside `citationsContent`, beside the citations.
                    content.push(read_bedrock_citations_content(cc));
                } else if let Some(reasoning) = block_val.get(super::REASONING_CONTENT) {
                    // A response can carry a `reasoningContent` block (the model's reasoning). As the
                    // request-side reader, map it onto IR `Thinking { text, signature }` via
                    // `read_bedrock_reasoning_block`; a future union member yields `None`.
                    if let Some(block) = read_bedrock_reasoning_block(reasoning) {
                        content.push(block);
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::THINKING,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping Converse response reasoningContent block with no decodable \
                             member (neither reasoningText nor redactedContent)"
                        );
                    }
                } else if let Some(image) = block_val.get(keys::IMAGE) {
                    // A response can carry an `image` block (model output or tool-rendered), decoded
                    // via `read_bedrock_image_block` as in the request-side readers: `source.bytes`
                    // (base64) and `source.s3Location` (on `IrImageSource::Vendor { vendor:
                    // "bedrock" }`). A block with neither yields `None`.
                    if let Some(block) = read_bedrock_image_block(image) {
                        content.push(block);
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::IMAGE,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping Converse response image block with no decodable source \
                             (neither source.bytes nor source.s3Location)"
                        );
                    }
                }
            }
        }

        let stop_reason_val = obj
            .get(super::STOP_REASON)
            .and_then(|s| s.as_str())
            .map(stop_reason_map);
        // The refinement beside the coarse reason (`model_context_window_exceeded` is `MaxTokens`
        // + `ContextWindowExceeded`).
        let stop_detail = obj
            .get(super::STOP_REASON)
            .and_then(|s| s.as_str())
            .and_then(stop_detail_map);

        // An absent `usage` falls back to zero counts, as the streaming path does, rather than
        // erroring: it is an upstream quirk (mock backend, future variant), and a spurious
        // `ClientError` would mislabel the cause and confuse retry logic.
        let usage_obj = obj.get(keys::USAGE);
        // `cacheDetails`, the per-TTL breakdown of `cacheWriteInputTokens`, rides the same table
        // (see `USAGE`). Absent is zero, a present but unreadable count refuses.
        let mut usage = read_bedrock_usage(usage_obj)?;
        if let Some(tier) = read_served_tier(body) {
            usage.detail.service_tier = Some(tier);
        }
        // The guardrail policy units AWS bills beside the tokens ride `trace`, not `usage`; they
        // travel on the usage under their open classes. An unreadable one refuses.
        usage.detail.open_units = guardrail_units(body)?;

        Ok(crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason: stop_reason_val,
            usage,
            // The Converse body has no `id`, `created` or `system_fingerprint`; its only identity is
            // the `x-amzn-RequestId` header, which this reader does not see. So every body-level
            // identity field is `None`, the faithful capture, and a Bedrock-to-Bedrock passthrough
            // reproduces the native id-less body.
            model: None,
            id: None,
            created: None,
            system_fingerprint: None,
            // Anthropic-on-Bedrock echoes the matched stop string under
            // `additionalModelResponseFields.stop_sequence`.
            stop_sequence: obj
                .get(super::ADDITIONAL_MODEL_RESPONSE_FIELDS)
                .and_then(|f| f.get(keys::STOP_SEQUENCE))
                .and_then(|v| v.as_str())
                .map(String::from),

            request_echo: None,
            stop_detail,
            safety: super::read_guardrail_verdicts(obj.get(TRACE)),
            ..Default::default()
        })
    }

    /// The frame's `:event-type` header is the `type` the reader dispatches on: usage rides only
    /// `metadata`, and a failure only the five modeled stream exceptions (the buffered stop reason
    /// of `messageStop` reaches no usage).
    fn same_proto_reads(&self, event_type: &str, _payload: &[u8]) -> bool {
        matches!(
            event_type,
            ET_METADATA
                | super::INTERNAL_SERVER_EXCEPTION
                | super::MODEL_STREAM_ERROR_EXCEPTION
                | super::THROTTLING_EXCEPTION
                | super::VALIDATION_EXCEPTION
                | super::SERVICE_UNAVAILABLE_EXCEPTION
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
