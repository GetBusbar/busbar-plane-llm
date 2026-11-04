use super::*;
use crate::codec::dialect::ir_parse_error;
use crate::codec::keys;

type BlockOrigin = crate::codec::ir::IrBlockOrigin;
type BlockSource = crate::codec::ir::IrBlockSource;
const SYSTEM_TEXT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system[].text",
};
const SYSTEM_GUARD: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "system[].guardContent",
};
const MESSAGE_TEXT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].text",
};
const MESSAGE_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolUse",
};
const MESSAGE_TOOL_RESULT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult",
};
const MESSAGE_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].image",
};
const MESSAGE_REASONING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].reasoningContent",
};
const MESSAGE_CITATIONS: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].citationsContent",
};
const MESSAGE_GUARD: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].guardContent",
};
const MESSAGE_DOCUMENT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].document",
};
const MESSAGE_VIDEO: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].video",
};
const MESSAGE_SEARCH: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].searchResult",
};
const RESULT_TEXT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].text",
};
const RESULT_JSON: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].json",
};
const RESULT_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].image",
};
const RESULT_DOCUMENT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].document",
};
const RESULT_VIDEO: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].video",
};
const RESULT_SEARCH: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "messages[].content[].toolResult.content[].searchResult",
};
const RESPONSE_TEXT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "output.message.content[].text",
};
const RESPONSE_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "output.message.content[].toolUse",
};
const RESPONSE_IMAGE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "output.message.content[].image",
};
const RESPONSE_REASONING: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "output.message.content[].reasoningContent",
};
const RESPONSE_CITATIONS: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "output.message.content[].citationsContent",
};
const STREAM_TOOL_USE: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockStart.start.toolUse",
};
const STREAM_TEXT_EVENT: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockStart",
};
const STREAM_TEXT_START: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockStart.start",
};
const STREAM_TEXT_DELTA: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockDelta.delta.text",
};
const STREAM_CITATION_DELTA: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockDelta.delta.citation",
};
const STREAM_REASONING_START: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockStart.start.reasoningContent",
};
const STREAM_REASONING_DELTA: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockDelta.delta.reasoningContent",
};
const STREAM_REDACTED_DELTA: BlockSource = BlockSource {
    dialect: VENDOR_NAME,
    path: "contentBlockDelta.delta.reasoningContent.redactedContent",
};

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

/// Smithy restJson1 error identity: suffix before namespace, preserving token case.
fn normalize_error_identity(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .split_once(':')
        .map_or(value, |(identity, _)| identity);
    let value = value
        .split_once('#')
        .map_or(value, |(_, identity)| identity)
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

impl ProtocolReader for BedrockReader {
    fn recover_truncated_usage(&self, tail: &[u8]) -> Option<busbar_contract::billing::TokenUsage> {
        let v = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"usage\"")?;
        if !TOKEN_USAGE_MEMBERS.iter().any(|k| v.get(*k).is_some()) {
            return None;
        }
        let mut usage = read_bedrock_usage(Some(&v)).ok()?;
        if let Some(trace) = super::super::usage_tail::isolate_tail_usage_object(tail, b"\"trace\"")
        {
            let mut holder = serde_json::Map::new();
            holder.insert(TRACE.to_string(), trace);
            usage.detail.open_units = guardrail_units(&serde_json::Value::Object(holder)).ok()?;
        }
        Some(usage.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        self.extract_error_with_head(status, &[], body)
    }

    fn extract_error_with_head(
        &self,
        status: StatusCode,
        head: busbar_contract::protocol::HeadFields<'_>,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        let json = crate::codec::json::parse::<serde_json::Value>(body).ok();
        let provider_code = json
            .as_ref()
            .and_then(|json| json.get(keys::MESSAGE))
            .and_then(|message| message.as_str())
            .map(String::from);
        let body_identity = |name: &str| {
            json.as_ref()
                .and_then(|json| json.get(name))
                .and_then(|value| value.as_str())
                .and_then(normalize_error_identity)
        };

        let structured_type = head
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(b"x-amzn-errortype"))
            .find_map(|(_, value)| {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(normalize_error_identity)
            })
            .or_else(|| body_identity(keys::CODE))
            .or_else(|| body_identity(super::DUNDER_TYPE));

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

        let mut extra = serde_json::Map::new();
        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        let mut system_cache_points: Vec<serde_json::Value> = Vec::new();
        let mut message_cache_points: Vec<serde_json::Value> = Vec::new();

        let mut system_guard_content: Vec<serde_json::Value> = Vec::new();
        let mut message_guard_content: Vec<serde_json::Value> = Vec::new();

        let mut message_doc_video: Vec<serde_json::Value> = Vec::new();

        let signature_origin_here =
            bedrock_signature_origin(obj.get("model").and_then(|m| m.as_str()));

        let mut system_blocks: Vec<crate::codec::ir::IrBlock> = Vec::new();
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();
        if let Some(system_arr) = obj.get(keys::SYSTEM).and_then(|s| s.as_array()) {
            for (idx, sys_val) in system_arr.iter().enumerate() {
                if let Some(text_val) = sys_val.get(keys::TEXT).and_then(|t| t.as_str()) {
                    system_blocks.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                    block_origins.system.push(BlockOrigin::at(&SYSTEM_TEXT));
                } else if let Some(cache_point) = sys_val.get(super::CACHE_POINT) {
                    system_cache_points.push(serde_json::json!({
                        (keys::I): idx,
                        (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                    }));

                    set_preceding_block_cache_control(&mut system_blocks);
                } else if let Some(guard_content) = sys_val.get(super::GUARD_CONTENT) {
                    let modelled = guard_content_block(guard_content);
                    system_guard_content.push(serde_json::json!({
                        (keys::I): idx,
                        (super::B): modelled.as_ref().map(|_| system_blocks.len()),
                        (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                    }));

                    if let Some(block) = modelled {
                        system_blocks.push(block);
                        block_origins.system.push(BlockOrigin::at(&SYSTEM_GUARD));
                    }
                }
            }
        }

        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(messages_val) = obj.get(keys::MESSAGES) {
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
                let mut msg_origins: Vec<BlockOrigin> = Vec::new();

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
                            msg_origins.push(BlockOrigin::at(&MESSAGE_TEXT));
                        } else if let Some(tool_use) = content_val.get(super::TOOL_USE_CAMEL) {
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

                                thought_signature: None,
                            });
                            msg_origins.push(BlockOrigin::at(&MESSAGE_TOOL_USE));
                        } else if let Some(tool_result) = content_val.get(super::TOOL_RESULT) {
                            let tu_id = tool_result
                                .get(super::TOOL_USE_ID)
                                .and_then(|id| id.as_str())
                                .unwrap_or("")
                                .to_string();

                            let mut inner_content: Vec<crate::codec::ir::IrBlock> = Vec::new();
                            let mut inner_origins: Vec<BlockOrigin> = Vec::new();
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
                                        inner_origins.push(BlockOrigin::at(&RESULT_TEXT));
                                    } else if let Some(json_val) = inner_val.get(keys::JSON) {
                                        inner_content.push(crate::codec::ir::IrBlock::Json(
                                            json_val.clone(),
                                        ));
                                        inner_origins.push(BlockOrigin::at(&RESULT_JSON));
                                    } else if let Some(image) = inner_val.get(keys::IMAGE) {
                                        if let Some(block) = read_bedrock_image_block(image) {
                                            inner_content.push(block);
                                            inner_origins.push(BlockOrigin::at(&RESULT_IMAGE));
                                        }
                                    } else if let Some(document) = inner_val.get(keys::DOCUMENT) {
                                        inner_content.push(bedrock_media_block(
                                            crate::codec::ir::IrMediaKind::Document,
                                            document,
                                        ));
                                        inner_origins.push(BlockOrigin::at(&RESULT_DOCUMENT));
                                    } else if let Some(video) = inner_val.get(super::VIDEO) {
                                        inner_content.push(bedrock_media_block(
                                            crate::codec::ir::IrMediaKind::Video,
                                            video,
                                        ));
                                        inner_origins.push(BlockOrigin::at(&RESULT_VIDEO));
                                    } else if let Some(sr) =
                                        inner_val.get(super::SEARCH_RESULT_CAMEL)
                                    {
                                        inner_content.push(super::read_bedrock_search_result(sr));
                                        inner_origins.push(BlockOrigin::at(&RESULT_SEARCH));
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
                            msg_origins.push(BlockOrigin {
                                source: Some(&MESSAGE_TOOL_RESULT),
                                children: inner_origins,
                            });
                        } else if let Some(image) = content_val.get(keys::IMAGE) {
                            if let Some(block) = read_bedrock_image_block(image) {
                                msg_content.push(block);
                                msg_origins.push(BlockOrigin::at(&MESSAGE_IMAGE));
                            }
                        } else if let Some(reasoning) = content_val.get(super::REASONING_CONTENT) {
                            if let Some(mut block) = read_bedrock_reasoning_block(reasoning) {
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
                                msg_origins.push(BlockOrigin::at(&MESSAGE_REASONING));
                            }
                        } else if let Some(cc) = content_val.get(super::CITATIONS_CONTENT) {
                            msg_content.push(read_bedrock_citations_content(cc));
                            msg_origins.push(BlockOrigin::at(&MESSAGE_CITATIONS));
                        } else if let Some(cache_point) = content_val.get(super::CACHE_POINT) {
                            message_cache_points.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (keys::BLOCK): { (super::CACHE_POINT): cache_point.clone() },
                            }));

                            set_preceding_block_cache_control(&mut msg_content);
                        } else if let Some(guard_content) = content_val.get(super::GUARD_CONTENT) {
                            let modelled = guard_content_block(guard_content);
                            message_guard_content.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): modelled.as_ref().map(|_| msg_content.len()),
                                (keys::BLOCK): { (super::GUARD_CONTENT): guard_content.clone() },
                            }));

                            if let Some(block) = modelled {
                                msg_content.push(block);
                                msg_origins.push(BlockOrigin::at(&MESSAGE_GUARD));
                            }
                        } else if let Some(document) = content_val.get(keys::DOCUMENT) {
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (keys::DOCUMENT): document.clone() },
                            }));

                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Document,
                                document,
                            ));
                            msg_origins.push(BlockOrigin::at(&MESSAGE_DOCUMENT));
                        } else if let Some(video) = content_val.get(super::VIDEO) {
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (super::VIDEO): video.clone() },
                            }));

                            msg_content.push(bedrock_media_block(
                                crate::codec::ir::IrMediaKind::Video,
                                video,
                            ));
                            msg_origins.push(BlockOrigin::at(&MESSAGE_VIDEO));
                        } else if let Some(sr) = content_val.get(super::SEARCH_RESULT_CAMEL) {
                            message_doc_video.push(serde_json::json!({
                                (keys::M): msg_idx,
                                (keys::I): block_idx,
                                (super::B): msg_content.len(),
                                (keys::BLOCK): { (super::SEARCH_RESULT_CAMEL): sr.clone() },
                            }));
                            msg_content.push(super::read_bedrock_search_result(sr));
                            msg_origins.push(BlockOrigin::at(&MESSAGE_SEARCH));
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

                            strict: tool_spec.get(keys::STRICT).and_then(|v| v.as_bool()),
                        });
                    } else if tool_val.get(super::CACHE_POINT).is_some() {
                        if let Some(last) = tools.last_mut() {
                            last.cache_control = Some(crate::codec::ir::CacheControl {
                                kind: crate::codec::ir::CacheKind::Ephemeral,
                            });
                        }
                    }
                }
            }
        }

        let tool_choice = read_bedrock_tool_choice(obj.get(keys::TOOL_CONFIG));

        let max_tokens = match obj.get(super::INFERENCE_CONFIG) {
            Some(config) => {
                let config = config.as_object().ok_or_else(ir_parse_error)?;
                config
                    .get(super::MAX_TOKENS_CAMEL)
                    .map(|value| {
                        value
                            .as_u64()
                            .filter(|&v| v > 0)
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or_else(ir_parse_error)
                    })
                    .transpose()?
            }
            None => None,
        };

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

        top_k_was_camel &= top_k.is_some();

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

        if top_k_was_camel {
            extra.insert(
                TOP_K_CAMEL_SENTINEL.to_string(),
                serde_json::Value::Bool(true),
            );
        }

        let reasoning = None;

        let response_format = read_bedrock_response_format(obj);

        let mut ir = crate::codec::ir::IrRequest {
            reasoning,
            system: system_blocks,
            block_origins,
            messages,
            tools,
            max_tokens,
            top_k,
            tool_choice,

            stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
            response_format,

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
                let Some(idx) = read_content_block_index(data) else {
                    return vec![IrStreamEvent::Error(busbar_contract::protocol::IrError {
                        class: StatusClass::ServerError,
                        provider_signal: Some("invalid Bedrock contentBlockIndex".into()),
                        retry_after: None,
                    })];
                };

                if let Some(start_obj) = data.get(keys::START).and_then(|s| s.as_object()) {
                    if let Some(tool_use) = start_obj
                        .get(super::TOOL_USE_CAMEL)
                        .and_then(|t| t.as_object())
                    {
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

                            state.open_tools.insert(idx);
                            state
                                .block_origins
                                .insert(idx, BlockOrigin::at(&STREAM_TOOL_USE));

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
                        state.thinking_block_open = true;
                        state
                            .block_origins
                            .insert(idx, BlockOrigin::at(&STREAM_REASONING_START));
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Thinking { kind: None },
                            refusal: false,
                        });
                    } else if start_obj.is_empty() && state.started && !state.text_block_open {
                        state.text_block_open = true;
                        state
                            .block_origins
                            .insert(idx, BlockOrigin::at(&STREAM_TEXT_START));
                        out.push(IrStreamEvent::BlockStart {
                            index: idx,
                            block: crate::codec::ir::IrBlockMeta::Text,
                            refusal: false,
                        });
                    }
                } else if state.started && !state.text_block_open {
                    state.text_block_open = true;
                    state
                        .block_origins
                        .insert(idx, BlockOrigin::at(&STREAM_TEXT_EVENT));
                    out.push(IrStreamEvent::BlockStart {
                        index: idx,
                        block: crate::codec::ir::IrBlockMeta::Text,
                        refusal: false,
                    });
                }
            }

            Some(ET_CONTENT_BLOCK_DELTA) => {
                let Some(idx) = read_content_block_index(data) else {
                    return vec![IrStreamEvent::Error(busbar_contract::protocol::IrError {
                        class: StatusClass::ServerError,
                        provider_signal: Some("invalid Bedrock contentBlockIndex".into()),
                        retry_after: None,
                    })];
                };

                if let Some(delta_obj) = data.get(keys::DELTA).and_then(|d| d.as_object()) {
                    if delta_obj.contains_key(keys::TEXT) {
                        let text_val = delta_obj
                            .get(keys::TEXT)
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();

                        if state.started && !state.text_block_open {
                            state.text_block_open = true;
                            state
                                .block_origins
                                .insert(idx, BlockOrigin::at(&STREAM_TEXT_DELTA));
                            out.push(IrStreamEvent::BlockStart {
                                index: idx,
                                block: crate::codec::ir::IrBlockMeta::Text,
                                refusal: false,
                            });
                        }

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
                        if state.started && !state.text_block_open {
                            state.text_block_open = true;
                            state
                                .block_origins
                                .insert(idx, BlockOrigin::at(&STREAM_CITATION_DELTA));
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
                        if state.started && !state.thinking_block_open {
                            state.thinking_block_open = true;

                            let block = if reasoning.contains_key(super::REDACTED_CONTENT) {
                                crate::codec::ir::IrBlockMeta::RedactedThinking
                            } else {
                                crate::codec::ir::IrBlockMeta::Thinking { kind: None }
                            };
                            let origin = if reasoning.contains_key(super::REDACTED_CONTENT) {
                                &STREAM_REDACTED_DELTA
                            } else {
                                &STREAM_REASONING_DELTA
                            };
                            state.block_origins.insert(idx, BlockOrigin::at(origin));
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
                        }
                    }
                }
            }

            Some(ET_CONTENT_BLOCK_STOP) => {
                let Some(idx) = read_content_block_index(data) else {
                    return vec![IrStreamEvent::Error(busbar_contract::protocol::IrError {
                        class: StatusClass::ServerError,
                        provider_signal: Some("invalid Bedrock contentBlockIndex".into()),
                        retry_after: None,
                    })];
                };

                if state.started {
                    if state.open_tools.remove(&idx) {
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.text_block_open {
                        state.text_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    } else if state.thinking_block_open {
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: idx });
                    }
                }
            }

            Some(ET_MESSAGE_STOP) => {
                let stop_reason = data.get(super::STOP_REASON).and_then(|s| s.as_str());
                state.pending_stop_reason = stop_reason.map(stop_reason_map);

                state.pending_stop_detail = stop_reason.and_then(stop_detail_map);
                state.pending_stop_sequence = data
                    .get(super::ADDITIONAL_MODEL_RESPONSE_FIELDS)
                    .and_then(|f| f.get(keys::STOP_SEQUENCE))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }

            Some(ET_METADATA) => {
                let usage_val = data.get(keys::USAGE);
                let mut usage = match read_bedrock_usage(usage_val) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };

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

                let class = super::stream_error_class(exc);
                out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                    class,
                    provider_signal: message.or_else(|| Some(exc.to_string())),
                    retry_after: None,
                }));
            }

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
        let mut block_origins = crate::codec::ir::IrBlockOrigins::default();

        {
            let content_arr = message_val
                .get(keys::CONTENT)
                .and_then(|v| v.as_array())
                .ok_or_else(ir_parse_error)?;
            for block_val in content_arr {
                if let Some(text_val) = block_val.get(keys::TEXT).and_then(|t| t.as_str()) {
                    content.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                    block_origins.response.push(BlockOrigin::at(&RESPONSE_TEXT));
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
                    block_origins
                        .response
                        .push(BlockOrigin::at(&RESPONSE_TOOL_USE));
                } else if let Some(cc) = block_val.get(super::CITATIONS_CONTENT) {
                    content.push(read_bedrock_citations_content(cc));
                    block_origins
                        .response
                        .push(BlockOrigin::at(&RESPONSE_CITATIONS));
                } else if let Some(reasoning) = block_val.get(super::REASONING_CONTENT) {
                    if let Some(block) = read_bedrock_reasoning_block(reasoning) {
                        content.push(block);
                        block_origins
                            .response
                            .push(BlockOrigin::at(&RESPONSE_REASONING));
                    } else {
                        let _native_drop_origin = crate::codec::drops::block_scope(Some(
                            &BlockOrigin::at(&RESPONSE_REASONING),
                        ));
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::THINKING,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [],
                            "dropping Converse response reasoningContent block with no decodable \
                             member (neither reasoningText nor redactedContent)"
                        );
                    }
                } else if let Some(image) = block_val.get(keys::IMAGE) {
                    if let Some(block) = read_bedrock_image_block(image) {
                        content.push(block);
                        block_origins
                            .response
                            .push(BlockOrigin::at(&RESPONSE_IMAGE));
                    } else {
                        let _native_drop_origin = crate::codec::drops::block_scope(Some(
                            &BlockOrigin::at(&RESPONSE_IMAGE),
                        ));
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

        let stop_detail = obj
            .get(super::STOP_REASON)
            .and_then(|s| s.as_str())
            .and_then(stop_detail_map);

        let usage_obj = obj.get(keys::USAGE);

        let mut usage = read_bedrock_usage(usage_obj)?;
        if let Some(tier) = read_served_tier(body) {
            usage.detail.service_tier = Some(tier);
        }

        usage.detail.open_units = guardrail_units(body)?;

        Ok(crate::codec::ir::IrResponse {
            logprobs: Vec::new(),
            role: crate::codec::ir::IrRole::Assistant,
            content,
            block_origins,
            stop_reason: stop_reason_val,
            usage,

            model: None,
            id: None,
            created: None,
            system_fingerprint: None,

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

#[cfg(test)]
#[path = "tests/reader_block_origin_tests.rs"]
mod block_origin_tests;
