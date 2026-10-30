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
    /// the first is the first candidate's, which `read_response` reads. Located structurally, so a
    /// `finishReason` key inside `content` (e.g. a model-written `functionCall.args` holding
    /// `"MALFORMED_FUNCTION_CALL"`) is never read as the stop reason and never faults a shared lane.
    /// `FIELD_FINISH_REASON`, quoted, as bytes so the lookup allocates nothing.
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
        // The same `USAGE` table as the complete response: thinking tokens fold into output and the
        // tool-use prompt term into input, so a truncated response counts like a complete one. An
        // unreadable count is refused alone and bills nothing. The stated-total check rides too, so
        // a recovered turn ledgers the same `unitemized_tokens` remainder.
        let mut usage = crate::codec::usage_count::read_usage(COUNT_LABEL, Some(&v), USAGE).ok()?;
        usage.detail.usage_identity_note = super::gemini_usage_identity_note(Some(&v));
        Some(usage.to_token_usage())
    }

    fn extract_error(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> busbar_contract::upstream::RawUpstreamError {
        // Parse once; `provider_code` and `structured_type` come from the same value.
        let json = crate::codec::json::parse::<serde_json::Value>(body).ok();
        let error_obj = json
            .as_ref()
            .and_then(|j| j.get(keys::ERROR_WORD))
            .and_then(|e| e.as_object());

        // `error.code` is a JSON integer (the HTTP status, per google.rpc.Status), e.g. `429`, so
        // `as_str()` would drop it and fall back to the gRPC status name, breaking comparisons
        // against numeric strings. Read the integer first and stringify it, tolerate a string-typed
        // `code` (some proxies), and use `status` only when `code` is absent.
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

        // Context-length-exceeded is a 400 `INVALID_ARGUMENT` whose message carries the overflow text
        // (no distinct google.rpc.Code). The raw `provider_code` is then the bare status, which the
        // breaker treats as a ClientError that penalizes the lane instead of failing over. Override it
        // with the canonical `context_length_exceeded` (breaker.rs, then StatusClass::ContextLength)
        // so failover is penalty-free. Mirrors `AnthropicReader::extract_error`. The lowercased raw
        // body is scanned, so the match is independent of which field carried the text; the substring
        // set mirrors `classify()` above.
        let provider_code = {
            // Status-gated to 400/413, as in `AnthropicReader::extract_error`. A 429 or 5xx whose body
            // quotes token-overflow phrasing (e.g. a retry-after message) must not become ContextLength,
            // which would hide a real rate-limit/server fault from the breaker. `or_else` keeps an
            // already-derived `provider_code` when the heuristic does not fire.
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

        // Gemini signals a dead egress credential (revoked, wrong or expired key, or one lacking
        // Generative Language API access) as a 400 `INVALID_ARGUMENT` or 403 `PERMISSION_DENIED` with
        // ErrorInfo `reason: API_KEY_INVALID` and a message like "API key not valid. Please pass a
        // valid API key." A bare 400 is a ClientFault that benches nothing, so the lane keeps getting
        // picked and serving a guaranteed rejection. Re-shape the raw error so the breaker classifies
        // Auth and parks the lane (HardDown, fail over). 401 is the Auth-classifying status in
        // `breaker::normalize_raw_error`, independent of the operator error-map; overriding
        // `http_status` is safe because the forwarder relays the ingress-native auth status to the
        // client (`auth_failure_status_and_kind`) and this value feeds only breaker classification.
        // `provider_code` is also set to the canonical `"auth"` (`breaker::status_class_from_str`).
        //
        // Precision: fires only on an explicit api-key-invalid signal (the `API_KEY_INVALID` reason in
        // ErrorInfo `details[].reason`, or an "api key (not/in)valid / expired" message), never on a
        // generic `INVALID_ARGUMENT` field-validation 400 (e.g. bad `contents[0].role`), which stays a
        // lane-healthy ClientFault.
        let (http_status, provider_code) = {
            let lower = String::from_utf8_lossy(body).to_lowercase();
            // The documented machine-readable reason is unambiguous on its own, regardless of status
            // name, and is read from the ErrorInfo field, not the body at large. Scanning the whole body
            // made the override with the harshest consequence (parking the lane, HardDown for every
            // tenant on that destination) the only ungated one: Gemini echoes request content in an
            // `INVALID_ARGUMENT` message, so a prompt containing the token could bench a healthy shared
            // credential for everyone with one request.
            let has_api_key_invalid_reason = error_obj
                .and_then(|e_obj| e_obj.get(FIELD_DETAILS))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|details| {
                    details.iter().any(|d| {
                        d.get(keys::REASON)
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|r| {
                                r.eq_ignore_ascii_case(GEMINI_ERROR_REASON_API_KEY_INVALID)
                            })
                    })
                });
            // A prose bad-key message: an explicit "api key" paired with a specific bad-key phrasing
            // ("API key not valid" / "API key … expired" / "invalid api key" / "api key is invalid").
            // A bare "invalid" fired on any `INVALID_ARGUMENT` validation 400 whose prose named an "api
            // key" and benched a healthy lane on a pure client error.
            let api_key_message = lower.contains("api key not valid")
                || lower.contains("api-key not valid")
                || lower.contains("invalid api key")
                || lower.contains("invalid api-key")
                || lower.contains("api key is invalid")
                || lower.contains("api-key is invalid")
                || lower.contains("api key expired")
                || lower.contains("api-key expired")
                || lower.contains("api key has expired")
                || lower.contains("api-key has expired");
            // The auth heuristic applies only under the google.rpc.Code statuses Gemini uses for
            // auth/permission failures. The documented reason is authoritative alone; the prose message
            // is trusted only under one of these.
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

        // Per-request tool-call index. `functionCall` parts carry no id, so a deterministic non-empty
        // one is synthesized (`synth_tool_call_id`); the index keeps two same-named calls distinct so
        // Anthropic/OpenAI egress gets a unique `id`/`tool_use_id`.
        let mut tool_call_index: usize = 0;
        // Pairs every functionResponse with the functionCall it answers (GEM-01); see its doc.
        let mut call_ledger = GeminiCallLedger::default();

        // systemInstruction is Gemini's system content.
        if let Some(sys_instr) = obj.get(FIELD_SYSTEM_INSTRUCTION) {
            if let Some(parts_arr) = sys_instr.get(FIELD_PARTS).and_then(|p| p.as_array()) {
                for part in parts_arr {
                    if let Some(text_val) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                        system_blocks.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                    }
                }
            }
        }

        // contents array (messages)
        let mut messages: Vec<crate::codec::ir::IrMessage> = Vec::new();
        if let Some(contents_val) = obj.get(FIELD_CONTENTS) {
            // Edge-validate `contents`: a present wrong-typed value is rejected with a 400 rather than
            // coerced to an empty conversation (as the openai_chat/cohere readers do). Absent stays lenient.
            let contents_arr = contents_val.as_array().ok_or_else(ir_parse_error)?;
            for (turn, content_val) in contents_arr.iter().enumerate() {
                let role_str = content_val
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let role = match role_str {
                    // `Content.role` is optional; absent or empty is an implicit user turn per the
                    // schema and SDK. Matches the streaming reader.
                    keys::USER | "" => crate::codec::ir::IrRole::User,
                    keys::MODEL => crate::codec::ir::IrRole::Assistant,
                    _ => return Err(ir_parse_error()),
                };

                let mut msg_content = Vec::new();
                // Edge-validate `parts` (array-only): a present wrong-typed value would silently become
                // an empty turn, so reject with a 400. Absent stays lenient.
                let parts_val = content_val.get(FIELD_PARTS);
                if let Some(pv) = parts_val {
                    if !pv.is_array() {
                        return Err(ir_parse_error());
                    }
                }
                if let Some(parts_arr) = parts_val.and_then(|p| p.as_array()) {
                    for part in parts_arr {
                        // Thinking part: `thought: true` carries reasoning text and an opaque
                        // `thoughtSignature`, read as IrBlock::Thinking (not Text) so a prior-turn
                        // reasoning block survives with its signature. Checked first as it also has `text`.
                        if part.get(FIELD_THOUGHT).and_then(|t| t.as_bool()) == Some(true) {
                            let text = part
                                .get(keys::TEXT)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            let signature = part
                                .get(FIELD_THOUGHT_SIGNATURE)
                                .and_then(|s| s.as_str())
                                // The signature is accepted verbatim; `redacted` is hardcoded `false`
                                // below, so a client cannot forge a redacted block via it.
                                .map(String::from);
                            // A Gemini thought part is a summary of the reasoning (IR-17). Its
                            // signature origin (IR-18) stays unknown: a history turn holds whatever the
                            // client was handed, and a client of a foreign backend echoes that backend's
                            // blob (the response writer relays it). Stamping Gemini would make that
                            // backend's writer drop its own signature next turn. A Gemini backend's
                            // response is stamped (`read_response`). Busbar's provenance envelope (a
                            // foreign signature wrapped for this client) restores the original bytes
                            // and origin; a bare signature stays unknown.
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
                        }
                        // Text part
                        else if let Some(text_val) = part.get(keys::TEXT).and_then(|t| t.as_str())
                        {
                            msg_content.push(crate::codec::ir::IrBlock::text(text_val.to_string()));
                        }
                        // FunctionCall (ToolUse)
                        else if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                            let name = func_call
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            // A zero-arg functionCall is `{}`, not `null`, matching the response readers.
                            let args = empty_object_if_absent(func_call.get(FIELD_ARGS));
                            // The call's native `id` (GEM-08) when present, else a stable synthesized one
                            // keyed by (index, name). No turn salt: `tool_call_index` is global across
                            // `contents` (see `synth_tool_call_id`) and advances for every call, so a
                            // synthesized id does not depend on whether an earlier call had a native one.
                            let id = gemini_call_id(func_call)
                                .map(str::to_string)
                                .unwrap_or_else(|| synth_tool_call_id(tool_call_index, &name, ""));
                            tool_call_index += 1;
                            call_ledger.record_call(&id, &name, turn);
                            // `thoughtSignature` is a sibling of `functionCall` on the `Part`, as for the
                            // `thought:true` block. Gemini 3 needs it echoed back verbatim next turn;
                            // captured here (harmless though a same-protocol path is a raw passthrough).
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
                        }
                        // FunctionResponse (ToolResult)
                        else if let Some(func_resp) = part.get(FIELD_FUNCTION_RESPONSE) {
                            let name = func_resp
                                .get(keys::NAME)
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let response_val = func_resp
                                .get(keys::RESPONSE)
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            // Response as string content
                            let response_text = crate::codec::json::to_string(&response_val)
                                .unwrap_or_else(|_| "unknown".to_string());
                            // The result's `tool_use_id` is the answered call's id (GEM-01): the
                            // response's own native `id` (GEM-08), else the positional pairing
                            // `GeminiCallLedger` documents. The writer resolves a call id back to the
                            // function name, so the name need not ride in `tool_use_id`.
                            let tool_use_id =
                                call_ledger.pair_response(gemini_call_id(func_resp), &name);
                            // Gemini documents `response.output` for a result and `response.error` for a
                            // failure (GEM-05): `error` without `output` is the failed call other dialects flag `is_error`.
                            let is_error = response_val.as_object().is_some_and(|o| {
                                o.get(keys::ERROR_WORD).is_some_and(|e| !e.is_null())
                                    && !o.contains_key(keys::OUTPUT)
                            });
                            let mut content = vec![crate::codec::ir::IrBlock::text(response_text)];
                            // A multimodal result's attachments ride in `functionResponse.parts` (GEM-07).
                            if let Some(parts) =
                                func_resp.get(FIELD_PARTS).and_then(|p| p.as_array())
                            {
                                content.extend(parts.iter().filter_map(read_gemini_media_part));
                            }
                            msg_content.push(crate::codec::ir::IrBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                                cache_control: None,
                            });
                        }
                        // InlineData (base64) / FileData (URI) to Image or Media by mime (`read_gemini_media_part`).
                        else if let Some(block) = read_gemini_media_part(part) {
                            msg_content.push(block);
                        }
                        // Any other part (`executableCode` / `codeExecutionResult`, a future kind) has no
                        // IR form: not read, and a translate attempt names the drop (`REQUEST_BLOCKS`).
                    }
                }

                messages.push(crate::codec::ir::IrMessage {
                    role,
                    content: msg_content,
                });
            }
        }

        // tools array (functionDeclarations)
        let mut tools: Vec<crate::codec::ir::IrTool> = Vec::new();
        let mut hosted_tools: Vec<crate::codec::ir::IrHostedTool> = Vec::new();
        if let Some(tools_arr) = obj.get(keys::TOOLS).and_then(|t| t.as_array()) {
            for tool_val in tools_arr {
                // functionDeclarations inside tools
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
                        // Two schema slots: `parametersJsonSchema` (JSON Schema, read as-is; once ignored,
                        // leaving a foreign target an empty schema, GEM-03) and the OpenAPI-subset
                        // `parameters` (normalized to JSON Schema, GEM-11).
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
                // `googleSearch` / `codeExecution` / `urlContext` cross as typed hosted tools (GEM-10,
                // IR-11); an unknown kind stays a raw hosted tool.
                let (typed, raw) = read_gemini_hosted_tools(tool_val);
                hosted_tools.extend(typed);
                tools.extend(raw);
            }
        }

        // `maxOutputTokens` is read as i64 with a bounds-checked `u32::try_from`, not `as u32`, which
        // would truncate a value above `u32::MAX` into a cap the caller never asked for. Out of range
        // drops to `None` and the backend applies its default.
        let max_tokens = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_MAX_OUTPUT_TOKENS))
            .and_then(|v| v.as_i64())
            .filter(|&v| v > 0)
            .and_then(|v| u32::try_from(v).ok());
        // The `generationConfig` sampling controls (temperature, topP, topK, stopSequences, penalties,
        // seed, candidateCount to `n`) are map-file rows read below; the raw `generationConfig` also
        // survives same-protocol via `extra`.
        // response_format: structured output is `generationConfig.responseMimeType` (+ optional
        // `responseSchema`), with no single native key. The IR carries a normalized
        // `{responseMimeType, responseSchema?}` with each present sub-field verbatim; deliberately
        // lossy in shape (cross-protocol writers map it to their own form) while Gemini to Gemini stays
        // byte-identical via the preserved `generationConfig` in `extra`. `None` when neither is present.
        let response_format = read_gemini_response_format(obj.get(FIELD_GENERATION_CONFIG));
        // Logprobs ask: `generationConfig.responseLogprobs` (boolean) and `.logprobs` (top-count,
        // 0-20), carried first-class so an OpenAI backend receives `logprobs`/`top_logprobs`.
        let logprobs = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_RESPONSE_LOGPROBS))
            .and_then(|v| v.as_bool());
        let top_logprobs = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(keys::LOGPROBS))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        // The thinking ask: `thinkingConfig.thinkingBudget` (token count; -1 = "model decides"),
        // promoted to carry to Anthropic's budget_tokens (number copy) or OpenAI's reasoning_effort
        // (bucketized). The raw thinkingConfig also survives same-protocol via `extra`; the writer
        // overlays a fresh one from the typed field cross-protocol.
        let thinking_config = obj
            .get(FIELD_GENERATION_CONFIG)
            .and_then(|gc| gc.get(FIELD_THINKING_CONFIG));
        let reasoning = match thinking_config
            .and_then(|tc| tc.get(FIELD_THINKING_BUDGET))
            .filter(|b| !b.is_null())
        {
            // A budget that is not an accepted int32 is the caller's error, answered in Gemini's
            // error envelope, never silently left out.
            Some(budget) => match read_gemini_thinking_budget(budget).ok_or_else(ir_parse_error)? {
                -1 => Some(crate::codec::ir::IrReasoningAsk::Dynamic),
                // 0 = thinking off. It carries no ask across the seam (`_ => None`): promoting it to
                // `Off` would switch thinking off on a foreign reasoning-by-default lane and change
                // what that request is billed. Dropped on the one drop path; the raw member still rides
                // `extra` same-dialect.
                0 => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [],
                        "dropping thinkingBudget 0 (reasoning off) across dialects: it carries no \
                         ask, as in 1.5.5"
                    );
                    None
                }
                n => Some(crate::codec::ir::IrReasoningAsk::Budget(n.unsigned_abs())),
            },
            // Gemini 3's `thinkingLevel` is the effort ask (GEM-09). The API takes one of the two, so
            // a present budget is the ask.
            None => thinking_config
                .and_then(|tc| tc.get("thinkingLevel"))
                .and_then(|v| v.as_str())
                .and_then(read_gemini_thinking_level),
        };
        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        // Promote `toolConfig.functionCallingConfig` into the IR `tool_choice` so a forced directive
        // survives the seam. The raw `toolConfig` also stays in `extra` (not in `modeled_keys`, like
        // `generationConfig`), keeping Gemini to Gemini byte-identical; the writer overlays a fresh
        // `functionCallingConfig` from the typed field.
        let tool_choice = read_gemini_tool_choice(obj.get(keys::TOOL_CONFIG));
        // The function-name subset the directive restricts to (IR-10).
        let allowed_tools = read_gemini_allowed_tools(obj.get(keys::TOOL_CONFIG));

        // Collect unmodeled top-level keys into `extra`. `model` is in the modeled set so the loop does
        // not re-insert it; it is preserved once by the explicit pre-insert below.
        //
        // `stream` is not in `modeled_keys`: a source body's `stream` is preserved through `extra` and
        // echoed by the writer, like `model`, so read to write stays byte-identical
        // (`test_gemini_roundtrip_identity`). `IrRequest.stream` is the source of truth for path
        // selection (`upstream_path_for_stream`); the `extra` copy is round-trip fidelity only. The
        // router-injected `stream`/shim never reach a real backend because
        // `proxy::strip_router_shim_keys` runs unconditionally (same- and cross-protocol).
        //
        // The `__busbar_gemini_json_array` shim is in `modeled_keys`, so it never enters `extra` or
        // gets re-emitted by a writer. It has no round-trip meaning (a native request never carries
        // it). It was once absent, so the cross-protocol rebuild (which bypasses the same-protocol-only
        // strip) swept it into `extra` and every writer leaked the router fingerprint to foreign
        // backends. The forward-layer strip and this exclusion now guard it twice.
        //
        // The modelled keys are the mapping file's top-level rows; `generationConfig` is not one (it
        // rides `extra` raw, overlaid by the writer).

        // `model` is modeled but preserved in `extra` for round-trip identity, once here (the loop skips it).
        if let Some(model_val) = obj.get(keys::MODEL) {
            extra.insert(keys::MODEL.to_string(), model_val.clone());
        }

        crate::codec::carry::keep_unmodelled(super::map::REQUEST, obj, &mut extra);

        let mut ir = crate::codec::ir::IrRequest {
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
            // Gemini `labels` (IR-03); the raw copy also stays in `extra` for same-protocol identity.
            metadata: read_gemini_labels(obj.get(FIELD_LABELS)),
            allowed_tools,
            hosted_tools,
            // `generationConfig.responseModalities` (IR-19); the raw copy rides `extra` too.
            output_modalities: read_gemini_response_modalities(obj.get(FIELD_GENERATION_CONFIG)),
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

        if data.as_str() == Some(crate::codec::dialect::SSE_DONE_SENTINEL) || !data.is_object() {
            return out;
        }

        // 0. Inline error envelope. A stream can deliver `{"error":{"code","message","status"}}`
        // (google.rpc.Status) as a 200-status SSE data chunk (e.g. an upstream RESOURCE_EXHAUSTED after
        // the connection is established), carrying no `candidates`. Without this arm the reader would
        // emit a bare `MessageStart` and EOF: no `IrStreamEvent::Error`, no terminal events, so the
        // client hangs and the breaker never sees it. proxy engine only converts HTTP-status-level
        // errors. As in the Bedrock reader's inline `*Exception` and Cohere's terminal `ERROR`, map
        // `error.status`/`error.code` to a `StatusClass` and push one `IrStreamEvent::Error` so the
        // writer ends the stream with a native error frame. Handled before the MessageStart/candidates
        // block so an error-only chunk emits no stray MessageStart.
        if let Some(error_obj) = data.get(keys::ERROR_WORD).and_then(|e| e.as_object()) {
            let status_str = error_obj.get(keys::STATUS).and_then(|s| s.as_str());
            let code = error_obj.get(keys::CODE).and_then(|c| c.as_u64());
            let class = gemini_error_status_class(status_str, code);
            let message = error_obj
                .get(keys::MESSAGE)
                .and_then(|m| m.as_str())
                .map(String::from)
                .or_else(|| status_str.map(String::from));
            out.push(IrStreamEvent::Error(busbar_contract::protocol::IrError {
                class,
                provider_signal: message,
                retry_after: None,
            }));
            return out;
        }

        // 0b. Prompt-blocked envelope. The prompt itself can be rejected: a chunk with a top-level
        // `promptFeedback.blockReason` (SAFETY/BLOCKLIST/PROHIBITED_CONTENT/OTHER), no `candidates`
        // and no `error`. Without this arm the reader would emit a bare `MessageStart` and EOF with no
        // closing events, hanging the client. Surface it as MessageStart once, then a `safety`
        // MessageDelta + MessageStop. Guarded on candidates-absent so a chunk with both is handled by
        // the candidate path. Handled before the MessageStart/candidates block.
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
                // Close blocks opened by earlier chunks before the terminal MessageDelta (a mid-stream
                // prompt-block can follow a text/tool chunk), mirroring the finishReason path, or the
                // IR stream is unbalanced.
                close_open_blocks(state, &mut out);
                // Billed counts: an unreadable one refuses and the stream ends in an error.
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

        // 1. MessageStart once, on the first chunk, capturing the stream identity (the same
        // `responseId`/`modelVersion` as the whole-response body) for same-protocol passthrough.
        // Gemini streams carry no `created`, so it stays `None` (not fabricated).
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

        // `candidateCount > 1` is legal. This reader collapses to `candidates[0]`, all a
        // cross-protocol (IR-rebuilt) hop can carry. A same-protocol relay re-emits upstream bytes
        // verbatim and keeps all N, but still runs this reader as a usage side channel, so the warning
        // is scoped to cross-protocol. Warned once per stream. Defense in depth: the engine rejects
        // candidateCount>1 up front on cross-protocol routes.
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

        // Process only the first candidate, as `read_response` reads `candidates[0]`. With
        // `candidateCount > 1` each chunk may carry N candidates with their own `finishReason`; looping
        // all of them emitted a full terminal sequence per candidate, so an Anthropic/OpenAI ingress
        // writer produced several `message_stop`/`message_delta` frames per stream (a protocol
        // violation and a tell) and drained `open_tools` N times. One candidate gives exactly one
        // terminal sequence and matches the non-streaming path.
        if let Some(candidate) = candidates.and_then(|cands| cands.first()) {
            // 2. Content parts (text + functionCall)
            if let Some(content) = candidate.get(keys::CONTENT) {
                let role_val = content
                    .get(keys::ROLE)
                    .and_then(|r| r.as_str())
                    .unwrap_or("");

                if role_val == keys::MODEL || role_val.is_empty() {
                    if let Some(parts_arr) = content.get(FIELD_PARTS).and_then(|p| p.as_array()) {
                        // A text block, when one opens, owns IR index 0 and tool blocks take 1..n; a
                        // tool-only stream starts tools at 0 (see the tool branch). The next tool index
                        // comes from persistent state (`open_tools`), not a per-chunk local, so indices
                        // stay stable across a response's SSE chunks.
                        for part in parts_arr {
                            // A `thought: true` part is streamed reasoning, not answer text (as the
                            // buffered reader treats it). Route it to a Thinking block at index 0 ahead
                            // of the answer, like the OpenAI reader; otherwise a Gemini backend's
                            // thinking leaked into the answer on every cross-protocol stream. Same gate
                            // as OpenAI: once the answer phase begins, index 0 is taken and a late thought
                            // part is dropped rather than corrupt block pairing.
                            if part.get(FIELD_THOUGHT).and_then(|t| t.as_bool()) == Some(true) {
                                if let Some(text) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                                    if !text.is_empty()
                                        && !state.text_block_open
                                        && state.open_tools.is_empty()
                                    {
                                        state.reasoning_seen = true;
                                        if !state.thinking_block_open {
                                            state.thinking_block_open = true;
                                            out.push(IrStreamEvent::BlockStart {
                                                index: 0,
                                                block: crate::codec::ir::IrBlockMeta::Thinking {
                                                    // IR-17: a Gemini thought part is a summary.
                                                    kind: Some(
                                                        crate::codec::ir::IrThinkingKind::Summary,
                                                    ),
                                                },
                                                refusal: false,
                                            });
                                        }
                                        out.push(IrStreamEvent::BlockDelta {
                                            index: 0,
                                            delta: crate::codec::ir::IrDelta::ThinkingDelta(
                                                text.to_string(),
                                            ),
                                        });
                                        if let Some(sig) = part
                                            .get(FIELD_THOUGHT_SIGNATURE)
                                            .and_then(|v| v.as_str())
                                        {
                                            out.push(IrStreamEvent::BlockDelta {
                                                index: 0,
                                                delta: crate::codec::ir::IrDelta::SignatureDelta(
                                                    sig.to_string(),
                                                ),
                                            });
                                        }
                                    }
                                }
                                continue;
                            }

                            // Text block. It claims the next free IR index by order of first appearance
                            // (thinking owns 0 when present, then the count of tool blocks opened), not a
                            // hardcoded 0, so a tool arriving before the first text never collides and
                            // the index is stable for the stream.
                            if let Some(text) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    // A text block closed earlier (by a functionCall) keeps its index for a
                                    // late citation (GEM-20); new text after it is a new block.
                                    let ti = match state.text_index {
                                        Some(ti) if !state.text_block_closed => ti,
                                        _ => {
                                            // A new text block: its citations are relative to where it begins in
                                            // the answer (GEM-16), via the character count kept as text grew.
                                            state.text_block_start = state.streamed_chars;
                                            state.claim_ir_index()
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
                                        out.push(IrStreamEvent::BlockStart {
                                            index: ti,
                                            block: crate::codec::ir::IrBlockMeta::Text,
                                            refusal: false,
                                        });
                                    }
                                    // The answer text so far, which a streamed citation's byte offsets index
                                    // into (GEM-16). Bounded by the translate cap default (32 MiB); it only
                                    // anchors citation offsets, so a fragment crossing the bound is not
                                    // recorded (offsets past it clamp).
                                    if state.streamed_text.len().saturating_add(text.len())
                                        <= busbar_contract::codec::TRANSLATE_BODY_MAX_BYTES_DEFAULT
                                    {
                                        extend_char_marks(
                                            &mut state.streamed_char_marks,
                                            &mut state.streamed_chars,
                                            state.streamed_text.len(),
                                            text,
                                        );
                                        state.streamed_text.push_str(text);
                                    }
                                    out.push(IrStreamEvent::BlockDelta {
                                        index: ti,
                                        delta: crate::codec::ir::IrDelta::TextDelta(
                                            text.to_string(),
                                        ),
                                    });
                                }
                            }

                            // FunctionCall (ToolUse); Gemini sends whole args, not streamed
                            if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                                let name_val = func_call
                                    .get(keys::NAME)
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                // Bound `state.open_tools` so an upstream streaming unbounded
                                // `functionCall` parts without a terminal path (see
                                // `MAX_GEMINI_TOOL_FRAMES`) cannot grow heap without bound. Past the cap
                                // the frame is neither recorded nor emitted: the next index derives from
                                // `open_tools.len()`, so an unrecorded frame would duplicate indices.
                                // Mirrors the Cohere reader's cap.
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
                                    // A tool block claims the next free IR index from the monotone counter
                                    // by order of first appearance, never reset, so a slot claimed before a
                                    // terminal path cannot be re-claimed after it (the defect the openai_chat
                                    // reader once had; see `claim_ir_index`). `text_base` is only for
                                    // `synth_tool_call_id` (the id space is unrelated to IR indices).
                                    // Recorded in `open_tools` so the finishReason handler emits a matching
                                    // BlockStop. Any earlier text part counts as position 0, open or closed
                                    // (`state.text_index.is_some()`), as in the buffered reader, so streamed
                                    // and buffered answers name a call alike.
                                    let text_base = usize::from(state.text_index.is_some());
                                    let ir_idx = state.claim_ir_index();
                                    state.open_tools.insert(ir_idx);

                                    // Close a still-open thinking block first (as the text arm does), or the
                                    // tool block opens while index 0 is still open; Anthropic's stream never
                                    // has two content blocks open at once.
                                    if state.thinking_block_open {
                                        state.thinking_block_open = false;
                                        out.push(IrStreamEvent::BlockStop { index: 0 });
                                    }
                                    // Close a still-open text block too (preamble-then-tool), or the text and
                                    // tool blocks overlap and break bracketing. Mirrors the finishReason text
                                    // close (`text_base` was captured pre-close). The closed block keeps its
                                    // index (latched `text_block_closed`) so a later citation still annotates
                                    // its text instead of opening a new empty block beside the tool (GEM-20).
                                    if state.text_block_open {
                                        state.text_block_open = false;
                                        state.text_block_closed = true;
                                        let ti = state.text_index.unwrap_or(0);
                                        out.push(IrStreamEvent::BlockStop { index: ti });
                                    }

                                    // A zero-arg `functionCall` omits `args` or sends `{}`; a missing one
                                    // defaults to `{}`, not `null`, which gave `"input": null` /
                                    // `"arguments": "null"` on Anthropic/OpenAI egress (strict SDKs expect an
                                    // object). An explicitly present `args` (even `null`) is kept verbatim.
                                    let args = empty_object_if_absent(func_call.get(FIELD_ARGS));

                                    // Gemini streams carry no tool-call id; synthesize a stable non-empty one
                                    // keyed by (tool-position, name) so Anthropic/OpenAI writers emit one on
                                    // `content_block_start`. Tool blocks occupy `text_base..text_base+n`, so
                                    // `ir_idx - text_base` is the 0-based position. Salted with the chunk's
                                    // `responseId` (repeated on every chunk) so two turns in one conversation
                                    // whose first call shares a name get different ids (see `synth_tool_call_id`).
                                    let turn_salt =
                                        data.get(FIELD_RESPONSE_ID).and_then(|v| v.as_str());
                                    // The native `id` when the model sent one (GEM-08).
                                    let id = gemini_call_id(func_call)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            synth_tool_call_id(
                                                ir_idx - text_base,
                                                &name_val,
                                                turn_salt.unwrap_or(""),
                                            )
                                        });
                                    out.push(IrStreamEvent::BlockStart {
                                        index: ir_idx,
                                        block: crate::codec::ir::IrBlockMeta::ToolUse {
                                            id,
                                            name: name_val.clone(),
                                        },
                                        refusal: false,
                                    });

                                    // NOTE: `IrBlockMeta::ToolUse` has no `thoughtSignature` slot and this arm
                                    // does not read it, on purpose. Same-protocol streams are relayed raw and
                                    // never decoded here, and a cross-protocol stream has nowhere to carry a
                                    // captured signature for the client's next request: that signature comes
                                    // from `IrReq::prepare_for_egress` (request-side sentinel injection), not
                                    // stream capture. Do not "fix" this as a gap without re-reading that flow.
                                    // Emit the whole args as one InputJsonDelta (Gemini does not stream functionCall).
                                    let args_str =
                                        crate::codec::json::to_string(&args).unwrap_or_default();
                                    out.push(IrStreamEvent::BlockDelta {
                                        index: ir_idx,
                                        delta: crate::codec::ir::IrDelta::InputJsonDelta(args_str),
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // 2b. Streaming citations. Gemini delivers grounding citations at the candidate level
            // (`candidates[].citationMetadata.citationSources[]`), typically on a late chunk (often the
            // `finishReason` one). `read_gemini_citations` fills the neutral fields and byte-exact `raw`
            // as in the non-stream path, and they go out as one `IrDelta::CitationsDelta` on the text
            // block's index (`state.text_index`, or the next free slot if no text part has appeared),
            // before the finishReason path closes the block. Without this arm a streamed citation was dropped.
            //
            // `startIndex`/`endIndex` are UTF-8 byte offsets into the full answer, the IR's are
            // characters (GEM-16): convert against the text streamed so far (`state.streamed_text`), as
            // the buffered path does against the full text. Metadata follows the text it cites, so the
            // anchor holds it; an offset past it clamps.
            // Gemini restates the list cumulatively on every chunk, so only the tail past the watermark
            // is emitted (the whole list made a client see the first citation once per chunk). A list
            // shorter than the watermark (an upstream that resets) yields no delta, never a panic.
            let all_citations = read_gemini_streamed_citations(
                candidate,
                &state.streamed_text,
                &state.streamed_char_marks,
            );
            // GEM-16: converted offsets index the whole streamed answer; the IR indexes the annotated
            // text block, so shift by where it began.
            let block_start = i64::try_from(state.text_block_start).unwrap_or(i64::MAX);
            let citations: Vec<crate::codec::ir::IrCitation> = all_citations
                .get(state.citations_emitted..)
                .unwrap_or_default()
                .iter()
                .cloned()
                .map(|mut c| {
                    if block_start > 0 {
                        c.start_index = c.start_index.map(|s| (s - block_start).max(0));
                        c.end_index = c.end_index.map(|e| (e - block_start).max(0));
                    }
                    c
                })
                .collect();
            state.citations_emitted = state.citations_emitted.max(all_citations.len());
            if !citations.is_empty() && state.text_block_closed && !state.text_block_open {
                // These sources cite text a functionCall already closed (GEM-20): the citation annotates
                // that block at its index, not a new empty text block beside the open tool.
                if let Some(ti) = state.text_index {
                    out.push(IrStreamEvent::BlockDelta {
                        index: ti,
                        delta: crate::codec::ir::IrDelta::CitationsDelta(citations),
                    });
                }
            } else if !citations.is_empty() {
                // Claim the text block's index from the monotone counter (`claim_ir_index`), like the text
                // arm; otherwise a citation/logprobs delta before the first text part could reuse an index
                // another block claimed and corrupt the cross-protocol index mapping.
                let ti = state.text_index.unwrap_or_else(|| state.claim_ir_index());
                if !state.text_block_open {
                    // Close a still-open thinking block first (as the text arm does), or two blocks are open.
                    if state.thinking_block_open {
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: 0 });
                    }
                    state.text_block_open = true;
                    state.text_index = Some(ti);
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

            // 2c. Streaming logprobs, carried at the candidate level (`candidates[].logprobsResult`).
            // Anchored like citations: the delta attaches to the text block's index (opening it if no
            // text part has arrived) so an OpenAI-dialect stream can re-emit `choices[].logprobs.content[]`.
            let stream_logprobs = read_gemini_logprobs(candidate.get(FIELD_LOGPROBS_RESULT));
            if !stream_logprobs.is_empty() {
                // Claim the text block's index from the monotone counter (`claim_ir_index`), like the text
                // arm, to avoid index collisions. A closed text block keeps its index only for a late
                // citation (GEM-20); anything opening a text block after it claims a fresh index.
                let ti = match state.text_index {
                    Some(ti) if !state.text_block_closed => ti,
                    _ => state.claim_ir_index(),
                };
                if !state.text_block_open {
                    state.text_block_closed = false;
                    // Close a still-open thinking block first (as the text arm does), or two blocks are open.
                    if state.thinking_block_open {
                        state.thinking_block_open = false;
                        out.push(IrStreamEvent::BlockStop { index: 0 });
                    }
                    state.text_block_open = true;
                    state.text_index = Some(ti);
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

            // 3. finishReason: close blocks, then MessageDelta + MessageStop
            if let Some(finish_reason_val) =
                candidate.get(FIELD_FINISH_REASON).and_then(|r| r.as_str())
            {
                // Map Gemini's full FinishReason set to canonical IR stop reasons (no Gemini-only token leaks).
                let mut stop_reason = map_gemini_finish_reason(finish_reason_val);

                // `FinishReason` has no TOOL_USE: a tool-call turn ends with STOP, mapped to `end_turn`.
                // But this turn emitted `functionCall` parts (in `state.open_tools`, drained just below)
                // and other readers emit `tool_use` for such turns, so promote `end_turn` to `tool_use`
                // (Anthropic relays it; OpenAI maps to `"tool_calls"`). The Gemini writer maps it back to
                // STOP, keeping same-protocol streaming lossless. Only a bare `end_turn` is promoted; a
                // tool call cut off by `max_tokens`/`safety` keeps that stronger reason.
                if stop_reason == crate::codec::ir::IrStopReason::EndTurn
                    && !state.open_tools.is_empty()
                {
                    stop_reason = crate::codec::ir::IrStopReason::ToolUse;
                }

                close_open_blocks(state, &mut out);

                // Parse usageMetadata if present. Billed counts: absent is zero, an unreadable one
                // refuses and the stream ends in an error instead of ledgering zero tokens.
                let usage = match read_gemini_usage(data) {
                    Ok(usage) => usage,
                    Err(refusal) => {
                        out.push(IrStreamEvent::Error(refusal));
                        return out;
                    }
                };

                // A failed generation ends as an error. `MALFORMED_FUNCTION_CALL` maps to
                // `IrStopReason::Error`, which no cross-protocol writer has a native token for, so the
                // terminal MessageDelta would render as success, `terminal_error()` would stay None and
                // the breaker would record no fault. Push the Error event too (as Cohere's `ERROR` does)
                // so the breaker sees it and the client gets a real error frame. The MessageDelta and
                // MessageStop still follow, so the stream is terminated and usage is still charged
                // (keyed off usage, not this event).
                if stop_reason == crate::codec::ir::IrStopReason::Error {
                    out.push(IrStreamEvent::Error(IrError {
                        // Gemini gives no code or message beside the finish token, so classify it as a
                        // transient server fault: the lane recovers via cooldown, not a permanent penalty.
                        class: busbar_contract::upstream::StatusClass::ServerError,
                        provider_signal: Some(finish_reason_val.to_string()),
                        retry_after: None,
                    }));
                }

                out.push(IrStreamEvent::MessageDelta {
                    stop_reason: Some(stop_reason),
                    // Gemini has no stop_sequence analog in its stream.
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
        _host: &dyn busbar_contract::codec::CodecHost,
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

        // At least one candidate is required.
        let candidates_val = obj.get(FIELD_CANDIDATES).ok_or_else(ir_parse_error)?;
        let candidates = candidates_val.as_array().ok_or_else(ir_parse_error)?;

        if candidates.is_empty() {
            return Err(ir_parse_error());
        }

        // N>1 (`candidateCount`, promoted from `n`) is legal, but the IR holds one candidate, so extras
        // are truncated on a cross-protocol hop. Warned so it is observable (same-proto keeps all).
        if candidates.len() > 1 {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::wire("candidates[]"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [candidates = candidates.len(),],
                "gemini response carried multiple candidates; only the first is translated cross-protocol (the rest are dropped)"
            );
        }

        let candidate = &candidates[0];

        // Parse content into `IrResponse.content`. `content` is absent on a safety/recitation-filtered
        // candidate (only `finishReason` + `safetyRatings`); treat it as empty and continue to the
        // `finishReason` mapping, as the streaming reader does. Hard-failing gave a spurious 500.
        let mut content: Vec<crate::codec::ir::IrBlock> = Vec::new();
        // A Google-Search-grounded answer: the IR hosted web-search record, ahead of the text (its
        // spans keep riding the text blocks' citations).
        content.extend(super::citations::read_grounding_record(candidate));
        // Per-response tool-call index feeding `synth_tool_call_id` (Gemini carries no tool id).
        let mut tool_call_index: usize = 0;
        // This response's `responseId`, salted into every synthesized tool-call id so a different turn
        // in the same conversation (a separate `read_response`, whose `tool_call_index` restarts at 0)
        // cannot synthesize the same id for a same-named first call (see `synth_tool_call_id`). No salt
        // if the response omits `responseId`.
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
                // Thinking part: a `thought: true` part carries the model's reasoning `text` plus an
                // opaque `thoughtSignature` (the google-genai SDK's `Part.thought_signature`). Read as
                // IrBlock::Thinking with its signature, not Text, so reasoning survives the seam (Anthropic
                // `thinking` / OpenAI reasoning) and round-trips Gemini to Gemini. Checked before the text
                // arm because a thought part also has `text`.
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
                    // A thought part is a summary of the reasoning (IR-17) and its signature was minted by
                    // Gemini (IR-18), unless it is busbar's provenance envelope, which restores the
                    // recorded bytes and origin.
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
                }
                // Text part → IrBlock::Text
                else if let Some(text) = part.get(keys::TEXT).and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        content.push(crate::codec::ir::IrBlock::text(text.to_string()));
                    }
                }

                // FunctionCall to IrBlock::ToolUse. No native id, so synthesize a stable non-empty one
                // keyed by (index, name); the writer ignores the ToolUse `id`, and Anthropic/OpenAI
                // egress needs one for correlation.
                if let Some(func_call) = part.get(FIELD_FUNCTION_CALL) {
                    let name_val = func_call
                        .get(keys::NAME)
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    // A zero-arg functionCall gives `{}`, not `null` (see the streaming reader's note).
                    let args = empty_object_if_absent(func_call.get(FIELD_ARGS));

                    // The native `id` when the model sent one (GEM-08), else synthesized.
                    let id = gemini_call_id(func_call)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            synth_tool_call_id(tool_call_index, &name_val, tool_id_turn_salt)
                        });
                    tool_call_index += 1;
                    // `thoughtSignature` is a sibling of `functionCall` on the `Part`, as for the thought
                    // block. Gemini 3 requires it echoed back verbatim on replay; captured so a Gemini
                    // round-trip keeps it.
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
                }

                // A generated image / audio part (`inlineData` / `fileData`) is answer content (GEM-17):
                // read into Image / Media so a target with a response attachment slot (Bedrock `image`) gets it.
                if let Some(block) = read_gemini_media_part(part) {
                    content.push(block);
                }

                // Any other part (`executableCode` / `codeExecutionResult`, a future kind) has no IR form;
                // a translate attempt names the drop (`RESPONSE_BLOCKS`).
            }
        }

        // Grounding citations. Gemini reports them at the candidate level
        // (`candidates[].citationMetadata.citationSources[]`) but the IR carries them on a Text block.
        // `startIndex`/`endIndex` are byte offsets into the full output text (every text part
        // concatenated), so a multi-part answer converts against the concatenation and routes each
        // citation to the block its span falls in, re-expressed relative to that block (as Anthropic's
        // `char_location`). `attach_gemini_citations_to_text_blocks` does both; a no-op with no Text
        // block or no citationMetadata.
        attach_gemini_citations_to_text_blocks(candidate, &mut content);

        // finishReason to stop_reason (Gemini to canonical)
        let stop_reason = candidate
            .get(FIELD_FINISH_REASON)
            .and_then(|r| r.as_str())
            // Canonical-map the full FinishReason set (`map_gemini_finish_reason`) so no Gemini-only token leaks.
            .map(map_gemini_finish_reason);

        // `FinishReason` has no TOOL_USE: a tool-call turn ends with STOP, mapped to `end_turn`. The IR
        // has `ToolUse` blocks and other readers emit `tool_use`, so promote a bare `end_turn` to
        // `tool_use` when a `ToolUse` block is present (Anthropic relays it, OpenAI maps to
        // `"tool_calls"`). The Gemini writer maps it back to STOP, keeping round-trips lossless.
        // `max_tokens`/`safety` etc. keep their stronger reason.
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

        // usageMetadata: promptTokenCount to input_tokens, candidatesTokenCount to output_tokens
        let usage = read_gemini_usage(body)?;

        // Gemini reports the serving model as `modelVersion` (fall back to `model`).
        let model = obj
            .get(FIELD_MODEL_VERSION)
            .or_else(|| obj.get(keys::MODEL))
            .and_then(|m| m.as_str())
            .map(String::from);

        // Capture identity so Gemini to Gemini passthrough preserves it. The body carries an opaque
        // `responseId` (the SDK's `response_id`) but no OpenAI-style `created`, so `created` stays
        // `None` (the writer omits it; a synthesized one would be fabricated). `system_fingerprint` and
        // `stop_sequence` have no analogue. A Vertex body's RFC3339 `createTime` is read into
        // `usage.detail.create_time` by `read_gemini_usage` (see that field's doc for why).
        let id = obj
            .get(FIELD_RESPONSE_ID)
            .and_then(|i| i.as_str())
            .map(String::from);

        // Per-token logprobs from `logprobsResult`, so an OpenAI-dialect caller gets `choices[].logprobs.content[]`.
        let logprobs = read_gemini_logprobs(candidate.get(FIELD_LOGPROBS_RESULT));

        Ok(crate::codec::ir::IrResponse {
            logprobs,
            role: crate::codec::ir::IrRole::Assistant,
            content,
            stop_reason,
            usage,
            model,
            id,
            // Vertex's `createTime` is the response creation time, carried as epoch `created` so a
            // foreign client sees when the backend answered, not a seam-stamped "now" (GEM-18). The
            // verbatim string still rides `usage.detail.create_time`.
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
