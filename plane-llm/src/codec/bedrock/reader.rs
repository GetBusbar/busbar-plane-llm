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
        map: super::map::STREAM_PATHS,
        code: super::STREAM_CODE,
        drops: super::STREAM_DROPS,
        defaults: &[],
        reroot: None,
    }),
    stream_keyed_by_event: true,
    block_kinds: super::IR_BLOCK_KINDS,
    request_code_names: super::REQUEST_CODE_NAMES,
};

impl ProtocolReader for BedrockReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        // The turn's `usage`, not a guardrail's. A guardrail's `invocationMetrics.usage` (policy
        // units) sits under `trace` and can follow the turn's `usage`; the last `"usage"` in the tail
        // would then name no token count and read as a zero-token turn. Walk back to the last `usage`
        // that names a token count; none means no recovery (the caller's floor), never zero. The
        // guardrail units are then read off the trailing `trace`, as buffered.
        const KEY: &[u8] = b"\"usage\"";
        let mut end = tail.len();
        while let Some(at) = tail[..end].windows(KEY.len()).rposition(|w| w == KEY) {
            if let Some(v) =
                super::super::usage_tail::isolate_tail_usage_object(&tail[at..end], KEY)
            {
                if TOKEN_USAGE_MEMBERS.iter().any(|k| v.get(*k).is_some()) {
                    // An unreadable billed count is refused alone and bills nothing; readable ones
                    // recover. The per-TTL cache-write split rides this `usage` too.
                    let mut usage = read_bedrock_usage(Some(&v)).ok()?;
                    // Guardrail policy units, as the buffered and streamed reads bill them. The
                    // answer's `trace` follows the turn's `usage`, so it is in the tail; a `trace`
                    // before it lies inside the output and is not the answer's.
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
        // Parse the body once. Errors carry text in `message` and the type in `__type` (e.g.
        // `ValidationException`), which the breaker's error_map keys on, so it comes from `__type`.
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
        // `ValidationException` with the signal in `message` ("Input is longer than the maximum number
        // of tokens allowed", "maximum-tokens … requested"). Surface `context_length_exceeded` so the
        // breaker (`code == "context_length_exceeded"`) maps it to `StatusClass::ContextLength` and
        // fails over without penalty, instead of a ClientError that penalizes the lane. Mirrors
        // `AnthropicReader::extract_error`; keep in sync with the `classify` helper below.
        //
        // Gated on a 400: Bedrock emits oversized-context only as `400 ValidationException`, and an
        // ungated scan would misread a 5xx body that merely quotes the phrasing as ContextLength and
        // mask an unhealthy lane.
        let provider_code = if status == StatusCode::BAD_REQUEST {
            let lower = String::from_utf8_lossy(body).to_lowercase();
            if lower.contains("input is longer than the maximum number of tokens")
                || (lower.contains(keys::MAXIMUM_TOKENS) && lower.contains(keys::REQUESTED))
                || (lower.contains("exceeds the maximum")
                    && (lower.contains(keys::TOKEN) || lower.contains(keys::CONTEXT)))
            {
                Some(crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH.to_string())
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

        // Collect unmodeled top-level fields into `extra` so a Bedrock-to-Bedrock passthrough
        // re-emits them (`write_request` merges `req.extra`); otherwise `topP`, `topK`,
        // `stopSequences`, `additionalModelRequestFields`, `guardrailConfig`,
        // `additionalModelResponseFieldPaths`, `performanceConfig`, `promptVariables` etc. are lost,
        // changing model behaviour. Mirrors the Gemini/Cohere readers. `stream` is route-injected into
        // `IrRequest.stream` and not echoed (a native body never carries it); other modeled keys are
        // re-serialised from the IR, so excluding them avoids a double-emit.
        //
        // `inferenceConfig` and `toolConfig` are deliberately not modeled out: only `maxTokens`/
        // `temperature` and `tools` are typed, and the rest (`stopSequences`, `topP`, `topK`,
        // `stopCriteria`, `toolChoice` and future fields) would be lost. The whole raw object goes into
        // `extra` and `write_request` overlays the typed fields, so a passthrough re-emits the rest
        // (e.g. `toolChoice: {any: {}}` forced tool use). The typed fields still feed cross-protocol egress.
        //
        // The modeled keys are a sorted `&'static` slice probed with `binary_search` (allocation-free,
        // faster than hashing for this size). It MUST stay sorted: keep alphabetical.
        let mut extra = serde_json::Map::new();
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        // Native `cachePoint` markers with their original absolute index, re-emitted in place on a
        // same-protocol passthrough. See `CACHE_POINTS_SENTINEL`.
        let mut system_cache_points: Vec<serde_json::Value> = Vec::new();
        let mut message_cache_points: Vec<serde_json::Value> = Vec::new();
        // Native `guardContent` markers, same stash shape; see `GUARD_CONTENT_SENTINEL`.
        let mut system_guard_content: Vec<serde_json::Value> = Vec::new();
        let mut message_guard_content: Vec<serde_json::Value> = Vec::new();
        // Native top-level `document` / `video` blocks, same stash shape; see `DOC_VIDEO_SENTINEL`.
        // They appear only in message `content`, not `system`.
        let mut message_doc_video: Vec<serde_json::Value> = Vec::new();

        // IR-18: the family that minted this conversation's signatures, from the model id in the body.
        let signature_origin_here =
            bedrock_signature_origin(obj.get("model").and_then(|m| m.as_str()));

        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        if let Some(system_arr) = obj.get(keys::SYSTEM).and_then(|s| s.as_array()) {
            for (idx, sys_val) in system_arr.iter().enumerate() {
                if let Some(text_val) = sys_val.get(keys::TEXT).and_then(|t| t.as_str()) {
                    system_blocks.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                } else if let Some(cache_point) = sys_val.get(super::CACHE_POINT) {
                    // No IR counterpart: stash the marker with its original index so a same-protocol
                    // passthrough keeps prompt caching enabled.
                    system_cache_points.push(serde_json::json!({
                        (keys::I): idx,
                        (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                    }));
                    // Also map the marker onto the preceding block's IR `cache_control`, since the
                    // stash is dropped on the cross-protocol seam (else a Bedrock to Anthropic hop
                    // loses the boundary). Additive: the writer suppresses the inline `cache_control`
                    // whenever the stash is present, so nothing double-emits.
                    set_preceding_block_cache_control(&mut system_blocks);
                } else if let Some(guard_content) = sys_val.get(super::GUARD_CONTENT) {
                    // The guardrail qualifiers have no IR counterpart: stash the block with its
                    // original index for verbatim re-emission (see `GUARD_CONTENT_SENTINEL`). `b` is the
                    // IR index of the modelled block below, which the writer matches to suppress its
                    // own emission (see the `document` arm).
                    let modelled = guard_content_block(guard_content);
                    system_guard_content.push(serde_json::json!({
                        (keys::I): idx,
                        (super::B): modelled.as_ref().map(|_| system_blocks.len()),
                        (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                    }));
                    // BED-02: the guarded span is prompt content, so model it too; a cross-protocol hop
                    // (stash cleared) still sends the caller's text.
                    system_blocks.extend(modelled);
                }
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
            // Edge-validate `messages`: a present wrong-typed value is rejected with a 400 rather than
            // coerced to an empty conversation (as the openai_chat/cohere readers do). Absent stays lenient.
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
                // Edge-validate `content` (array-only on Converse): a present wrong-typed value would
                // silently become an empty turn, so reject with a 400. Absent stays lenient.
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
                            // A `toolUse` needs a non-empty string `toolUseId`, the key a later
                            // `toolResult` pairs against. Reject rather than invent one.
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
                                        // A `{"json": <value>}` tool-result block is structured data;
                                        // carried as `IrBlock::Json` so same-protocol egress re-emits
                                        // `{"json": ...}` rather than a text string.
                                        inner_content.push(crate::codec::ir::IrBlock::Json(
                                            json_val.clone(),
                                        ));
                                    } else if let Some(image) = inner_val.get(keys::IMAGE) {
                                        // `ToolResultContentBlock` also includes `image` (and
                                        // `document`/`video`); decoded symmetric with the writer,
                                        // which emits an `image` inside a toolResult.
                                        if let Some(block) = read_bedrock_image_block(image) {
                                            inner_content.push(block);
                                        }
                                    } else if let Some(document) = inner_val.get(keys::DOCUMENT) {
                                        // BED-05: the `document` member is the same DocumentBlock as
                                        // the top-level one, so it models as the same IR `Media`.
                                        inner_content.push(bedrock_media_block(
                                            crate::codec::ir::IrMediaKind::Document,
                                            document,
                                        ));
                                    } else if let Some(video) = inner_val.get(super::VIDEO) {
                                        // BED-05: likewise the `video` member.
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
                            // Decode `source.bytes` (base64) and `source.s3Location`, the two
                            // `ImageSource` members. An s3 image is carried as
                            // `IrImageSource::Vendor { vendor: "bedrock", value: {format, s3Location} }`
                            // (see mod.rs) so same-protocol egress re-emits it.
                            if let Some(block) = read_bedrock_image_block(image) {
                                msg_content.push(block);
                            }
                        } else if let Some(reasoning) = content_val.get(super::REASONING_CONTENT) {
                            // A `reasoningContent` block maps to `Thinking { text, signature }`
                            // (mirroring anthropic.rs `thinking`). Bedrock requires the signed
                            // reasoning echoed back on the follow-up turn, so dropping it diverges
                            // from a direct call. A future union member yields `None` (undecoded).
                            // `redacted` is set only on a genuine `redactedContent` member, so a client
                            // cannot forge it via `reasoningText.signature`.
                            if let Some(mut block) = read_bedrock_reasoning_block(reasoning) {
                                // IR-18: the signature's family is visible only through the model id.
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
                            // BED-01: a cited assistant turn carries its text inside
                            // `citationsContent`; without this arm the turn arrived empty.
                            msg_content.push(read_bedrock_citations_content(cc));
                        } else if let Some(cache_point) = content_val.get(super::CACHE_POINT) {
                            // No IR counterpart: stash the marker with its (message, block) index so
                            // a same-protocol passthrough keeps prompt caching enabled.
                            message_cache_points.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                            }));
                            // Also map onto the preceding block's IR `cache_control` so the boundary
                            // survives a Bedrock to Anthropic hop (see `set_preceding_block_cache_control`);
                            // the writer suppresses inline emission while the stash is present.
                            set_preceding_block_cache_control(&mut msg_content);
                        } else if let Some(guard_content) = content_val.get(super::GUARD_CONTENT) {
                            // Stash the guardrail block with its (message, block) index for verbatim
                            // re-emission (see `GUARD_CONTENT_SENTINEL`). `b` is the IR index of the
                            // modelled block below (see the `document` arm).
                            let modelled = guard_content_block(guard_content);
                            message_guard_content.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): modelled.as_ref().map(|_| msg_content.len()),
                                (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                            }));
                            // BED-02: model the guarded text/image as prompt content so a
                            // cross-protocol hop carries it (suppressed when the stash is present).
                            msg_content.extend(modelled);
                        } else if let Some(document) = content_val.get(keys::DOCUMENT) {
                            // A `document` block has no IR counterpart: stash it verbatim with its
                            // (message, block) index so a same-protocol passthrough re-emits it in
                            // place (`DOC_VIDEO_SENTINEL`).
                            // Two index spaces: `i` is the wire position (where the writer splices the
                            // raw block back); `b` is the modelled block's position in the IR content
                            // list (what the writer matches to suppress its own emission). They diverge
                            // once a wire slot with no IR block (`cachePoint`, `guardContent`) precedes
                            // this one; matching wire to IR index would miss the suppression and send
                            // the document upstream twice.
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (keys::DOCUMENT): document.clone() },
                            }));
                            // Also model it for cross-protocol hops: the stash only returns on
                            // Bedrock to Bedrock (the seam clears `extra`), so the caller's PDF would
                            // otherwise vanish silently on every other egress. The writer suppresses
                            // this block when the stash is present, so there is no double emission.
                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Document,
                                document,
                            ));
                        } else if let Some(video) = content_val.get(super::VIDEO) {
                            // A `video` block likewise: stash verbatim (`DOC_VIDEO_SENTINEL`); `b` is
                            // the modelled IR index, `i` the wire slot (see the `document` arm).
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (super::VIDEO): video.clone() },
                            }));
                            // Modelled for the cross-protocol hop too, as for `document`.
                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Video,
                                video,
                            ));
                        } else if let Some(sr) = content_val.get(super::SEARCH_RESULT_CAMEL) {
                            // A caller's `searchResult` (RAG passage): stashed verbatim in the same
                            // splice store for Bedrock to Bedrock, and modelled as the search-result
                            // slot cross-protocol (suppressed when the stash is present). See the
                            // `document` arm for `i` versus `b`.
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
                            // BED-08: `toolSpec.strict` is the same per-tool switch the IR carries.
                            strict: tool_spec.get(keys::STRICT).and_then(|v| v.as_bool()),
                        });
                    } else if tool_val.get(super::CACHE_POINT).is_some() {
                        // A `cachePoint` in `toolConfig.tools` marks the cache boundary for the tool
                        // definitions before it (Anthropic puts the same breakpoint on a tool). It maps
                        // to the preceding tool's `cache_control`, the sole carrier (no positional
                        // stash); same-protocol egress re-emits the marker from it. A leading
                        // cachePoint has nothing to attach to and is dropped.
                        //
                        // Accepted: a leading or doubled adjacent cachePoint does not byte-round-trip.
                        // AWS rejects those shapes (an empty or duplicate prefix), so no valid request
                        // loses fidelity.
                        if let Some(last) = tools.last_mut() {
                            last.cache_control = Some(crate::codec::ir::CacheControl {
                                kind: crate::codec::ir::CacheKind::Ephemeral,
                            });
                        }
                    }
                }
            }
        }

        // Promote `toolConfig.toolChoice` into the IR union so a forced directive survives the seam.
        let tool_choice = read_bedrock_tool_choice(obj.get(keys::TOOL_CONFIG));

        let max_tokens = if let Some(inference_config) =
            obj.get(super::INFERENCE_CONFIG).and_then(|i| i.as_object())
        {
            inference_config
                .get(super::MAX_TOKENS_CAMEL)
                .and_then(|v| v.as_u64())
                .filter(|&v| v > 0)
                // Bounds-checked: `as u32` would wrap a value above u32::MAX into a cap the caller
                // never asked for. Out-of-range becomes None (backend default). Mirrors the Gemini reader.
                .and_then(|v| u32::try_from(v).ok())
        } else {
            None
        };

        // `inferenceConfig.{temperature, topP, stopSequences}` are map-file rows (read below) and
        // also kept verbatim in the raw `inferenceConfig` in `extra`; the writer overlays the typed
        // fields onto it. `topK` is not an inferenceConfig field.
        // `top_k` lives in model-specific `additionalModelRequestFields` as `top_k` (Anthropic-on-
        // Bedrock and others) or `topK`. Either populates the IR field so it survives the seam (where
        // `extra` is cleared); the raw object stays in `extra` and the writer overlays `top_k` back.
        // `top_k_was_camel` records the `topK` spelling so a same-protocol passthrough round-trips
        // byte-identically instead of renaming it.
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
        // Only when a usable top_k came from the camel key; otherwise reset so the sentinel is not stamped.
        top_k_was_camel &= top_k.is_some();

        // Stash captured `cachePoint` markers under the sentinel so `write_request` re-emits them in
        // place on a same-protocol passthrough. Inserted only when present, keeping cache-free bodies byte-exact.
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

        // Stash captured `guardContent` markers under the sentinel, only when present.
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

        // Stash captured `document` / `video` markers under the sentinel, only when present.
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

        // Stamp the hint when top_k arrived as `topK`, so the writer re-emits `topK` same-protocol.
        // `extra` is cleared on the seam, so cross-protocol egress emits `top_k`. Only when a usable value resulted.
        if top_k_was_camel {
            extra.insert(
                TOP_K_CAMEL_SENTINEL.to_string(),
                serde_json::Value::Bool(true),
            );
        }

        // The reasoning asks in `additionalModelRequestFields` (Anthropic-on-Bedrock `thinking` and
        // `output_config.effort`, Nova `reasoningConfig`) are not promoted (`reasoning: None`): that
        // would switch on billed thinking on a foreign lane for a request billed without it. The raw
        // object stays in `extra` for passthrough; the one registered crossing, Converse's top-level
        // `outputConfig.effort` (CHANGELOG 1.6.0), is the map file's row.
        let reasoning = None;
        // BED-08: Converse's native structured output, `outputConfig.textFormat`.
        let response_format = read_bedrock_response_format(obj);

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            system: system_blocks,
            messages,
            tools,
            max_tokens,
            top_k,
            tool_choice,
            // A native Converse body has no `stream` field (the endpoint selects it), so the Bedrock
            // ingress route injects `"stream": true` for converse-stream (see `ingress_path_model`).
            // It must flow into the IR or the egress writer never produces a streaming body. False
            // when absent; a no-op same-protocol.
            stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
            response_format,
            // IR-03: `requestMetadata` crosses as typed metadata; the raw object stays in `extra`.
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
                        // A BlockStart must never precede its MessageStart (malformed or reordered
                        // stream): skip it, mirroring the text branch's `state.started` guard.
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

                            // Record the opened index (as the Gemini/OpenAI/Cohere readers do) so the
                            // matching `contentBlockStop` can verify a BlockStart happened.
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
                        // Some Bedrock-compatible backends send `contentBlockStart` with an empty
                        // `reasoningContent` start object. Open the Thinking block here so the deltas
                        // attach; the delta arm sees the flag set and does not re-open. (Native AWS
                        // `ContentBlockStart` only models `toolUse`, so real streams open on the first delta.)
                        state.thinking_block_open = true;
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                            refusal: false,
                        });
                    } else if start_obj.is_empty() && state.started && !state.text_block_open {
                        // Only an empty `start: {}` opens a Text block. A `start` with an unrecognized
                        // key (e.g. a future `image`/`reasoningContent`) is skipped, not mis-opened as
                        // text. Mirrors the Gemini/Cohere readers.
                        state.text_block_open = true;
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Text,
                            refusal: false,
                        });
                    }
                } else if state.started && !state.text_block_open {
                    // No `start` object: a text block.
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

                        // Lazily open the Text block on the first text delta. The native
                        // `ContentBlockStart$start` union only models `toolUse`, so a real stream sends no
                        // start for text; without the lazy open, plain text produced an orphan delta at
                        // index 0. An explicit empty-`start` already set the flag.
                        if state.started && !state.text_block_open {
                            state.text_block_open = true;
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block: crate::codec::ir::IrBlockMeta::Text,
                                refusal: false,
                            });
                        }

                        // Emit only once the text block is open; a delta before `messageStart` is dropped
                        // as an orphan, like the reasoningContent arm.
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
                        // Tool-input deltas are valid only in a block `contentBlockStart` opened (in
                        // `open_tools`); otherwise drop rather than emit an orphan.
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
                        // BED-01: a `citation` delta (one `CitationsDelta` per frame) interleaves with
                        // the text deltas of the block it cites at the same `contentBlockIndex`. Open the
                        // text block if the citation came first, as the text arm does.
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
                        // Extended thinking arrives as `reasoningContent` on `contentBlockDelta`, with no
                        // `contentBlockStart`: open the Thinking block on the first delta, then emit the
                        // matching delta. The union has three members:
                        //   - `text`            to ThinkingDelta
                        //   - `signature`       to SignatureDelta (the opaque token)
                        //   - `redactedContent` to RedactedReasoningDelta (opaque bytes). A typed delta
                        //     keeps the redacted block to one IR delta and one frame, so the writer
                        //     re-emits `redactedContent` without a plaintext leak; non-Bedrock writers drop it.
                        if state.started && !state.thinking_block_open {
                            state.thinking_block_open = true;
                            // A redacted block's only member is `redactedContent`, so peek it at open time
                            // to open with `RedactedThinking`, letting a cross-protocol writer (Anthropic)
                            // emit a native `redacted_thinking` start. The bytes ride the
                            // `RedactedReasoningDelta` emitted just below.
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
                            // A future member with none of the three keys carries no IR delta: the block
                            // stays open and it is skipped, mirroring the buffered reader's `None` arm.
                        }
                    }
                }
            }

            Some(ET_CONTENT_BLOCK_STOP) => {
                let idx = clamp_content_block_index(data);

                // A BlockStop must never precede its MessageStart nor be emitted unless some block is
                // known open. A malformed, reordered or duplicate `contentBlockStop` (before
                // `messageStart`, or for an index never started, notably a tool-use index) would give a
                // BlockStop with no BlockStart, violating the unbalanced-stream invariant (INV-A) and
                // serializing an invalid `content_block_stop` on e.g. Anthropic egress. Skipped silently.
                if state.started {
                    // Check `open_tools` first. Tool blocks are tracked by index, which is more
                    // precise than the index-blind text/thinking flags (valid only if one block of
                    // any kind is open at a time). A matching tool index is then never misattributed
                    // to a flag, nor leaves a stale entry to swallow a later text/thinking STOP, and
                    // concurrently open tool blocks close independently.
                    if state.open_tools.remove(&idx) {
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.text_block_open {
                        // Clear `text_block_open` on any stop while text is open, not only index 0: text
                        // after a tool-use sits at index > 0, and an `idx == 0` guard left the flag set
                        // and suppressed every later text block. At most one text block is open at a time.
                        state.text_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.thinking_block_open {
                        // Clear the thinking flag on its stop so a later reasoning block opens cleanly.
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    }
                    // else: no text/thinking/tool-use block is known open for `idx` — an
                    // unmatched/out-of-order/duplicate contentBlockStop. Silently skip, matching the
                    // START arms' policy for a frame that doesn't fit the established state.
                }
            }

            Some(ET_MESSAGE_STOP) => {
                // Bedrock splits the stop reason (`messageStop` frame) from the token usage (a
                // following `metadata` frame). To emit ONE combined `MessageDelta{stop_reason, usage}`
                // — so a cross-protocol ingress (e.g. Anthropic) sees the SINGLE `message_delta` a
                // native non-Bedrock stream carries, instead of two (the previous behavior was a
                // detectable tell) — we BUFFER the stop_reason here and pair it with the usage when
                // `metadata` arrives (see below). The combined delta is emitted from the `metadata`
                // branch.
                //
                // The terminal `MessageStop` is also DEFERRED to the `metadata` branch and emitted
                // AFTER the combined `MessageDelta`. The combined delta carries stop_reason + usage and
                // must precede the terminal stop in IR order, so that a non-eventstream ingress writer
                // (e.g. Anthropic) emits `message_delta` BEFORE `message_stop` — the native order. If
                // the `MessageStop` were emitted here (on `messageStop`, which arrives BEFORE
                // `metadata`), the IR order would be MessageStop-then-MessageDelta and the Anthropic
                // ingress would write `message_stop` before `message_delta` — a wrong, detectable
                // ordering. A bedrock->bedrock round-trip is unaffected: the `MessageStop` IR event
                // maps to no wire frame (`BedrockWriter` returns `None`), and the combined delta is
                // re-split into the native `messageStop` + `metadata` frame pair by `StreamTranslate`.
                let stop_reason = data.get(super::STOP_REASON).and_then(|s| s.as_str());
                state.pending_stop_reason = stop_reason.map(stop_reason_map);
                // IR-16 (BED-10) and BED-11: the refinement and the matched stop string ride this
                // frame too, and are buffered with the reason for the combined delta.
                state.pending_stop_detail = stop_reason.and_then(stop_detail_map);
                state.pending_stop_sequence = data
                    .get(super::ADDITIONAL_MODEL_RESPONSE_FIELDS)
                    .and_then(|f| f.get(keys::STOP_SEQUENCE))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }

            Some(ET_METADATA) => {
                // Usage trails the stop reason (Bedrock sends `metadata` after `messageStop`). Pair it
                // with the stop_reason buffered from the preceding `messageStop` frame into ONE
                // combined MessageDelta, so a cross-protocol ingress emits a single `message_delta`/
                // usage event (native fidelity) rather than two. A bedrock->bedrock round-trip re-splits
                // this combined delta back into the native `messageStop` + `metadata` frame pair in the
                // writer (`BedrockWriter::write_response_event` fan-out, driven by `StreamTranslate`).
                //
                // The terminal `MessageStop` is emitted HERE, AFTER the combined delta, so the IR order
                // is delta-then-stop and the ingress writer emits its native `message_delta` then
                // `message_stop` (delta-before-stop ordering). It is pushed unconditionally
                // (even when `metadata` carries no `usage`) so the downstream stream always receives its
                // terminal frame once `metadata` arrives.
                // Emit the combined MessageDelta UNCONDITIONALLY — even when `metadata` carries no
                // `usage` object. Native AWS Bedrock always sends `usage` here, but a mock /
                // Bedrock-compatible backend (common in staging & integration tests) may omit it. The
                // old code took `pending_stop_reason` only INSIDE the `usage` guard, so a usage-less
                // `metadata` dropped the buffered stop_reason entirely and terminated the stream with a
                // bare MessageStop — no preceding MessageDelta. For a Bedrock→Anthropic translation that
                // is a protocol-ordering violation (the Anthropic SDK expects `message_delta` before
                // `message_stop`) AND a silent loss of the stop_reason. We therefore build a usage from
                // whatever the frame carries (zero when absent — harmless) and always emit the delta,
                // consuming the buffered stop_reason, BEFORE the terminal MessageStop. A bare
                // `metadata` with neither usage nor a buffered stop_reason yields a zero-usage,
                // stop_reason-less delta, which is benign.
                // Billed counts: absent is zero, a present-but-unreadable count refuses, so the stream
                // ends in an error instead of ledgering zero tokens (cache counts and the per-TTL split
                // included; an unreadable cache count used to read as "no cache"). The per-TTL split
                // rides the `metadata` frame as it does the buffered `usage` (the TTLs price differently).
                let usage_val = data.get(keys::USAGE);
                let mut usage = match read_bedrock_usage(usage_val) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };
                // Guardrail policy units ride the same frame's `trace` as buffered, on the usage under
                // their open classes. An unreadable one refuses.
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

            // Mid-stream exception events. The `responseStream` union has exactly five modeled error
            // members (`internalServerException`, `modelStreamErrorException`, `validationException`,
            // `throttlingException`, `serviceUnavailableException`). `modelTimeoutException` is
            // request-level and never appears here (see `bedrock_stream_exception_for`). A recognized
            // event becomes `IrStreamEvent::Error` so the ingress writer ends the client stream with a
            // protocol-shaped error instead of leaving it hanging.
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
                // Map each of the five exception strings to its StatusClass, co-located with the
                // string set rather than a `_ => ServerError` default. The `other =>` arm is kept
                // (not `_`) only because `&str` matches are never exhaustive; the outer pattern is the guard.
                let class = match exc {
                    super::THROTTLING_EXCEPTION => StatusClass::RateLimit,
                    super::VALIDATION_EXCEPTION => StatusClass::ClientError,
                    super::SERVICE_UNAVAILABLE_EXCEPTION => StatusClass::Overloaded,
                    super::INTERNAL_SERVER_EXCEPTION | super::MODEL_STREAM_ERROR_EXCEPTION => {
                        StatusClass::ServerError
                    }
                    // Unreachable given the outer guard. A named binding (no `_`, per the no-catch-all
                    // rule) keeps the arm explicit; ServerError is the safe class.
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

            // Any other event type is skipped: this is the wire demux for an open-ended,
            // vendor-extensible stream, so a future event must not error.
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

        // Documented cross-protocol drop (drop, warn, test). A Converse response can carry
        // Bedrock-only members with no IR home or cross-protocol analog: `trace` (`guardrail`,
        // `promptRouter`), `additionalModelResponseFields` and `performanceConfig`. A same-protocol
        // hop keeps them via the proxy short-circuit (this reader is not called); this runs only on
        // cross-protocol egress, where they drop, so warn once per present field.
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
                    // BED-01: a cited answer carries its text inside `citationsContent`; without this
                    // arm the answer text was deleted.
                    content.push(read_bedrock_citations_content(cc));
                } else if let Some(reasoning) = block_val.get(super::REASONING_CONTENT) {
                    // A `reasoningContent` block is the model's reasoning output; map it as the
                    // request-side reader does (`read_bedrock_reasoning_block`). A future union member
                    // yields `None` (undecoded).
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
                    // An assistant `image` block (model image output / tool-rendered image), decoded
                    // as the request-side readers do via `read_bedrock_image_block` (`bytes` base64
                    // or `s3Location` as a `Vendor` source). Neither source yields `None`.
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
        // IR-16 (BED-10): the refinement beside the coarse reason (`model_context_window_exceeded`
        // is `MaxTokens` + `ContextWindowExceeded`).
        let stop_detail = obj
            .get(super::STOP_REASON)
            .and_then(|s| s.as_str())
            .and_then(stop_detail_map);

        // An absent `usage` is zero-defaulted, as the streaming path does: an upstream quirk, not a
        // client error, which a `ClientError` would mislabel.
        let usage_obj = obj.get(keys::USAGE);
        // `cacheDetails` rides the same table as the totals (see `USAGE`). Absent is zero, a
        // present-but-unreadable count refuses.
        let mut usage = read_bedrock_usage(usage_obj)?;
        if let Some(tier) = read_served_tier(body) {
            usage.detail.service_tier = Some(tier);
        }
        // Guardrail policy units ride `trace`, not `usage`, on the usage under their open classes.
        // An unreadable one refuses.
        usage.detail.open_units = guardrail_units(body)?;

        Ok(crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason: stop_reason_val,
            usage,
            // The Converse body has no `id`, `created` or `system_fingerprint`; the only identity is
            // the `x-amzn-RequestId` header, which is not in the body. So body-level identity is
            // `None`, which is what Bedrock sends, and a passthrough reproduces the id-less body.
            model: None,
            id: None,
            created: None,
            system_fingerprint: None,
            // BED-11: Anthropic-on-Bedrock echoes the matched stop string under
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

    /// `:event-type` is the `type` the reader dispatches on: usage rides only `metadata`, a failure
    /// only the five modeled stream exceptions.
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
