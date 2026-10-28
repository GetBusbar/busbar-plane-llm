use super::*;
use crate::codec::keys;

impl ProtocolWriter for AnthropicWriter {
    /// No image-fidelity slot: an image's `detail` is dropped, with the seam's warn.
    fn carries_image_detail(&self) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }

    fn upstream_path(&self) -> &str {
        PATH_UPSTREAM
    }

    /// IR-18: an Anthropic `signature` is read back as Claude's.
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        origin == crate::codec::ir::IrSignatureOrigin::Anthropic
    }

    /// Replace only the minted top-level `request_id`, and only where the writer put one, with the
    /// id the caller's entropy produces; every other byte is the writer's.
    fn write_error_from_entropy(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
        entropy: &[u8],
    ) -> serde_json::Value {
        let mut envelope = self.write_error(host, status, kind, message);
        if let Some(obj) = envelope.as_object_mut() {
            if obj.contains_key(super::REQUEST_ID) {
                obj.insert(
                    super::REQUEST_ID.to_string(),
                    serde_json::Value::String(super::request_id_from_entropy(entropy)),
                );
            }
        }
        envelope
    }

    fn write_error(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        // Native error envelope: `{"type":"error","error":{"type":<kind>,"message":<msg>}}`; the
        // SDK decodes `error.type` into the typed exception. Generic router `kind`s map to
        // Anthropic's vocabulary; an unrecognized `kind` passes through verbatim.
        //
        // Status first: native Anthropic reports upstream overload (503 or 529) as
        // `overloaded_error`, never `api_error`, and the wire status is the authoritative signal.
        if status == STATUS_OVERLOADED || status == STATUS_ANTHROPIC_OVERLOADED {
            return Self::error_envelope(host, ERR_TYPE_OVERLOADED, message);
        }
        let anthropic_type = match kind {
            // Generic router/auth/forward `kind`s → Anthropic's typed error vocabulary.
            "invalid_request" | "bad_request" => ERR_TYPE_INVALID_REQUEST,
            "authentication" | keys::UNAUTHORIZED => ERR_TYPE_AUTHENTICATION,
            "permission" | "forbidden" => ERR_TYPE_PERMISSION,
            "not_found" => ERR_TYPE_NOT_FOUND,
            ERR_TYPE_REQUEST_TOO_LARGE | "payload_too_large" => ERR_TYPE_REQUEST_TOO_LARGE,
            "rate_limit" | "too_many_requests" => ERR_TYPE_RATE_LIMIT,
            // Billing exhaustion: the router's quota kind aliases OpenAI's `insufficient_quota`,
            // which is not in the Anthropic error union; the billing member is `billing_error`.
            busbar_contract::protocol::KIND_INSUFFICIENT_QUOTA | "quota_exceeded" => {
                ERR_TYPE_BILLING
            }
            // Context overflow: `context_length_exceeded` is a reader-synthesized provider code,
            // not an Anthropic type; real Anthropic returns `invalid_request_error`.
            busbar_contract::protocol::PROVIDER_CODE_CONTEXT_LENGTH
            | busbar_contract::protocol::DISPOSITION_CONTEXT_LENGTH => ERR_TYPE_INVALID_REQUEST,
            busbar_contract::protocol::KIND_OVERLOADED => ERR_TYPE_OVERLOADED,
            busbar_contract::protocol::KIND_TIMEOUT => ERR_TYPE_TIMEOUT,
            ERR_TYPE_API_ERROR | busbar_contract::protocol::KIND_SERVER_ERROR | "internal" => {
                ERR_TYPE_API_ERROR
            }
            // Already Anthropic-native or unmapped: emit unchanged.
            other => other,
        };
        Self::error_envelope(host, anthropic_type, message)
    }

    fn attach_error_response_headers(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        headers: &mut busbar_contract::http::HeaderMap,
        _kind: &str,
        envelope: &serde_json::Value,
    ) {
        // A real response always carries the id in the `request-id` header (the SDK reads it
        // there, not from the body). Mirror the minted body `request_id` into it so they agree;
        // omitting it is a proxy tell.
        if let Some(rid) = envelope.get(super::REQUEST_ID).and_then(|v| v.as_str()) {
            if let Ok(hv) = busbar_contract::http::HeaderValue::from_str(rid) {
                headers.insert(HDR_REQUEST_ID, hv);
            }
        }
    }

    fn ingress_response_request_id(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        upstream_request_id: Option<&str>,
    ) -> Option<(&'static str, String)> {
        // The captured upstream `request-id` verbatim on a same-protocol passthrough, else a
        // synthesized `req_…` id. Synthesis failure omits the header.
        upstream_request_id
            .map(String::from)
            .or_else(|| synth_anthropic_request_id(host))
            .map(|id| (HDR_REQUEST_ID, id))
    }

    fn reshape_for_path_base(&self, body: &mut serde_json::Value) -> bool {
        // Claude on Vertex: the model rides the URL (`:rawPredict`), so the body omits `model`
        // and carries `anthropic_version`. This mutates the body, so a same-protocol passthrough
        // to such a lane is never pristine.
        match body.as_object_mut() {
            Some(obj) => {
                obj.remove(keys::MODEL);
                obj.insert(
                    "anthropic_version".to_string(),
                    serde_json::json!("vertex-2023-10-16"),
                );
                true
            }
            None => false,
        }
    }

    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        self.dropped_egress_controls_for_lane(req, &LaneCaps::default())
    }

    fn dropped_egress_controls_for_lane(
        &self,
        req: &crate::codec::ir::IrRequest,
        caps: &LaneCaps,
    ) -> Vec<&'static str> {
        // Mirrors the `write_request` warns: `frequency_penalty`/`presence_penalty`/`seed`/`n` have
        // no Anthropic form and are dropped. A schema-carrying `response_format` is not listed
        // (it projects onto `output_config.format`); a schema-less `json_object` is.
        let mut dropped = Vec::new();
        // A NATIVE-structured-output lane drops only a schema-less JSON mode; every other lane
        // drops the directive.
        if req
            .response_format
            .as_ref()
            .is_some_and(|rf| rf.json && (!caps.native_structured_output || rf.schema.is_none()))
        {
            dropped.push(keys::RESPONSE_FORMAT);
        }
        dropped.extend(
            crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
                .map(crate::codec::carry::Slot::name),
        );
        // Every hosted tool that crossed the seam is dropped on Anthropic egress.
        dropped.extend(req.hosted_tools.iter().map(|h| h.kind_str()));
        dropped
    }

    fn write_request(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
    ) -> serde_json::Value {
        self.write_request_for_lane(host, req, "", &LaneCaps::default())
    }

    fn write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        // There is no `system` role inside `messages`. Anthropic's reader promotes a wire system
        // message into `req.system`, but a cross-protocol IR can still carry an `IrRole::System`
        // message; fold its blocks into the top-level system array so `write_message` never
        // emits `role:"system"` (a 400).
        let mut system_blocks: Vec<&crate::codec::ir::IrBlock> = req.system.iter().collect();
        for msg in &req.messages {
            if msg.role == crate::codec::ir::IrRole::System {
                system_blocks.extend(msg.content.iter());
            }
        }
        if !system_blocks.is_empty() {
            let system_array: Vec<_> = system_blocks.into_iter().map(write_block).collect();
            out.insert(
                keys::SYSTEM.to_string(),
                serde_json::Value::Array(system_array),
            );
        }
        // Splice back raw blocks `read_request` parked because the IR cannot model them (see
        // `ANTHROPIC_UNMODELED_BLOCKS_SENTINEL`). Empty when absent, and always for a
        // cross-protocol IR since `prepare_for_egress` clears `extra`.
        let unmodeled_sentinel: &[serde_json::Value] = req
            .extra
            .get(ANTHROPIC_UNMODELED_BLOCKS_SENTINEL)
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let messages_array: Vec<_> = req
            .messages
            .iter()
            .filter(|msg| msg.role != crate::codec::ir::IrRole::System)
            .enumerate()
            .map(|(m, msg)| write_message(msg, m, unmodeled_sentinel))
            .collect();
        out.insert(
            keys::MESSAGES.to_string(),
            serde_json::Value::Array(messages_array),
        );
        // `strict` is carried natively, so no drop warn here.
        //
        // A tool subset (`allowed_tools`) has no `tool_choice` form, so it is expressed by
        // omission: only the listed function tools are sent.
        //
        // A hosted tool that crossed the seam (a Responses `web_search`, a Chat
        // `web_search_options`, a web fetch or code execution) is dropped with a warn. Writing it
        // as an Anthropic server tool would run Anthropic searches and charge the key
        // `search_units` it was never charged.
        crate::codec::dialect::warn_dropped(
            req.hosted_tools.iter().map(|h| h.kind_str()),
            &super::map::DROP_WARN,
        );
        let tools_array: Vec<_> = req
            .tools
            .iter()
            .filter(|t| {
                t.hosted.is_some()
                    || req
                        .allowed_tools
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&t.name))
            })
            .filter_map(write_tool)
            .collect();
        if !tools_array.is_empty() {
            out.insert(
                keys::TOOLS.to_string(),
                serde_json::Value::Array(tools_array),
            );
        }
        // `tool_choice` in the native object shape. The parallelism carry (OpenAI
        // `parallel_tool_calls`) rides the same object as the inverted `disable_parallel_tool_use`,
        // valid on auto/any/tool, not on `none`.
        if let Some(tc) = &req.tool_choice {
            // Anthropic 400s on a `tool_choice` with no `tools`. Reachable cross-protocol:
            // `prepare_for_egress` strips hosted tools, so a Responses web_search-only request can
            // arrive with `tools == []` and `tool_choice` set. Drop with a warn.
            if !out.contains_key(keys::TOOLS) {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Anthropic egress: Anthropic rejects a tool_choice with \
                     no tools array (likely because the hosted tools that carried it were stripped \
                     on the cross-protocol seam)");
            } else {
                let mut tc_val = write_anthropic_tool_choice(tc);
                if let (Some(parallel), Some(map)) =
                    (req.parallel_tool_calls, tc_val.as_object_mut())
                {
                    if map.get(keys::TYPE).and_then(|t| t.as_str()) != Some(keys::NONE_WORD) {
                        map.insert(
                            super::DISABLE_PARALLEL_TOOL_USE.to_string(),
                            serde_json::json!(!parallel),
                        );
                    }
                }
                out.insert(keys::TOOL_CHOICE.to_string(), tc_val);
            }
        } else if let Some(parallel) = req.parallel_tool_calls {
            // No directive but parallelism was set: it only fits inside a tool_choice object, so
            // synthesize the neutral `auto` carrier, and only when tools are present.
            if out.contains_key(keys::TOOLS) {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    serde_json::json!({(keys::TYPE): keys::AUTO, (super::DISABLE_PARALLEL_TOOL_USE): !parallel}),
                );
            }
        }
        // `output_config` holds both native structured outputs (`format`) and the reasoning
        // effort word (`effort`); built up below, emitted once.
        let mut output_config = serde_json::Map::new();
        // response_format uses native structured outputs, not a synthetic forced tool (which
        // overwrote the caller's `tool_choice`, lost the schema when thinking was on, and 400s on
        // models that reject forced tool use). The native slot constrains the answer, leaves tool
        // selection to the caller, and needs no mapping back on either path.
        //
        // Only a schema-carrying directive has a native form: a schema-less `json_object` is
        // dropped with a warn and reported through `dropped_egress_controls`, with no invented
        // schema. Whether the lane supports the native form is `LaneCaps::native_structured_output`
        // (it 400s on models that predate it); the default is the drop.
        if caps.native_structured_output {
            if let Some(rf) = req.response_format.as_ref().filter(|rf| rf.json) {
                match &rf.schema {
                    Some(schema) => {
                        let mut schema = schema.clone();
                        close_object_schemas(&mut schema);
                        // OpenAI's `json_schema.description` has no sibling slot; the schema's own
                        // `description` keyword carries it when the schema lacks one.
                        if let (Some(desc), Some(obj)) = (&rf.description, schema.as_object_mut()) {
                            obj.entry(keys::DESCRIPTION)
                                .or_insert_with(|| serde_json::json!(desc));
                        }
                        output_config.insert(
                            keys::FORMAT.to_string(),
                            serde_json::json!({ (keys::TYPE): OUTPUT_FORMAT_JSON_SCHEMA, (keys::SCHEMA): schema }),
                        );
                    }
                    None => {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::RESPONSE_FORMAT,
                            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                            [parameter = keys::RESPONSE_FORMAT, ],
                            "dropping schema-less JSON mode on Anthropic egress: structured outputs \
                             require a JSON schema and the Messages API has no schema-less JSON mode \
                             (lossy-by-target)");
                    }
                }
            }
        } else if req.response_format.as_ref().is_some_and(|rf| rf.json) {
            // A lane without native structured outputs drops the directive with a warn. A
            // synthetic forced tool is not its mapping: it overwrote the caller's `tool_choice`,
            // and a streamed answer reached the client as a tool call instead of JSON content.
            crate::codec::drops::writer_drop!(
                crate::codec::drops::RESPONSE_FORMAT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [parameter = keys::RESPONSE_FORMAT,],
                "dropping response_format on Anthropic egress: this lane does not declare native \
                 structured outputs (lossy-by-target)"
            );
        }
        if let Some(max_tokens) = req.max_tokens {
            out.insert(keys::MAX_TOKENS.to_string(), serde_json::json!(max_tokens));
        }
        // The reasoning carry: project the IR ask into `thinking`.
        //
        // A word-form ask (OpenAI `reasoning_effort`, Responses `reasoning.effort`, Anthropic
        // `output_config.effort`) or a "model decides" ask (Gemini `thinkingBudget:-1`) projects
        // onto adaptive thinking plus `output_config.effort`, the only on-mode Opus 4.7+/5.x,
        // Sonnet 5 and Fable accept (`budget_tokens` 400s there). A numeric ask keeps its number
        // as `budget_tokens`, clamped to leave >=1024 tokens of answer under max_tokens
        // (`budget_tokens < max_tokens`) and floored at the API's 1024 minimum; when max_tokens
        // cannot fit any thinking the ask is dropped with a warn. Anthropic also rejects modified
        // temperature/top_p/top_k alongside thinking, so those are omitted (warned) below.
        let mut thinking_emitted = false;
        match req.reasoning {
            // Reasoning off is matched first: it has no budget, and the caller's "off" must reach
            // a reasoning-by-default model. It is not an emitted ask, so the sampling knobs stay.
            // A lane whose model cannot switch thinking off (`LaneCaps::thinking_always_on`)
            // rejects `{type:"disabled"}`: the ask is omitted with a warn.
            Some(crate::codec::ir::IrReasoningAsk::Off) if caps.thinking_always_on => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "omitting reasoning OFF on Anthropic egress: this lane's model cannot switch \
                     thinking off (thinking_always_on) and rejects thinking.type \"disabled\""
                );
            }
            Some(crate::codec::ir::IrReasoningAsk::Off) => {
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_DISABLED }),
                );
            }
            Some(crate::codec::ir::IrReasoningAsk::Effort(effort))
                if caps.anthropic_adaptive_thinking =>
            {
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_ADAPTIVE }),
                );
                output_config.insert(
                    keys::EFFORT.to_string(),
                    serde_json::json!(anthropic_effort_word(effort)),
                );
                thinking_emitted = true;
            }
            Some(crate::codec::ir::IrReasoningAsk::Dynamic) if caps.anthropic_adaptive_thinking => {
                // "The model decides" is adaptive thinking; no effort word is invented.
                out.insert(
                    keys::THINKING.to_string(),
                    serde_json::json!({ (keys::TYPE): THINKING_TYPE_ADAPTIVE }),
                );
                thinking_emitted = true;
            }
            // "The model decides" on a lane without adaptive thinking: `budget_tokens` has no such
            // value, so the ask is dropped (no table entry stands in) and the model runs at default.
            Some(crate::codec::ir::IrReasoningAsk::Dynamic) => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "dropping a \"model decides\" reasoning ask on Anthropic egress: this lane does \
                     not declare adaptive thinking and budget_tokens has no dynamic form"
                );
            }
            // A numeric ask on every lane, and a word ask on a lane without adaptive thinking
            // (`LaneCaps::anthropic_adaptive_thinking` false; Haiku 4.5 / Sonnet 4.5 / Opus 4.5 and
            // older): `budget_tokens`, a word projected through the operator's effort table.
            Some(ask) => {
                let table = req
                    .reasoning_budgets
                    .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
                // `Off` and `Dynamic` are matched above; every other ask has a table entry.
                if let Some(want) = ask.to_budget(table) {
                    let cap = req.max_tokens.map(|mt| mt.saturating_sub(1024));
                    let budget = cap.map_or(want, |c| want.min(c));
                    if budget >= 1024 {
                        if budget != want {
                            crate::codec::drops::writer_drop!(
                                crate::codec::drops::REASONING,
                                &crate::codec::diagnostics::IR_DROP_REASONING,
                                [requested_budget = want, clamped_budget = budget, max_tokens = ?req.max_tokens, ],
                                "thinking budget clamped to fit under max_tokens");
                        }
                        out.insert(
                            keys::THINKING.to_string(),
                            serde_json::json!({(keys::TYPE): keys::ENABLED, (keys::BUDGET_TOKENS): budget}),
                        );
                        thinking_emitted = true;
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::REASONING,
                            &crate::codec::diagnostics::IR_DROP_REASONING,
                            [max_tokens = ?req.max_tokens, ],
                            "dropping reasoning ask on Anthropic egress: max_tokens leaves no room for \
                             the 1024-token thinking minimum");
                    }
                }
            }
            None => {}
        }
        if !output_config.is_empty() {
            out.insert(
                keys::OUTPUT_CONFIG.to_string(),
                serde_json::Value::Object(output_config),
            );
        }
        if thinking_emitted {
            // Anthropic 400s a forced or targeted tool_choice (`any` / `tool`) alongside extended
            // thinking; only `auto`/`none` are allowed. tool_choice was written above, so downgrade
            // `any`/`tool` to `auto` here, keeping `disable_parallel_tool_use`, with a warn.
            if let Some(tc) = out
                .get_mut(keys::TOOL_CHOICE)
                .and_then(|v| v.as_object_mut())
            {
                let ty = tc.get(keys::TYPE).and_then(|t| t.as_str());
                if ty == Some(keys::ANY) || ty == Some(keys::TOOL) {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::TOOL_CHOICE,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [tool_choice = ?ty, ],
                        "downgrading forced/targeted tool_choice to 'auto' on Anthropic egress: \
                         not compatible with thinking");
                    tc.insert(keys::TYPE.to_string(), serde_json::json!(keys::AUTO));
                    tc.remove(keys::NAME); // `name` is only valid on `{type:"tool"}`
                }
            }
        }
        // temperature (clamped to [0.0, 1.0]) / top_p / top_k / stop_sequences. Beside an emitted
        // thinking ask Anthropic 400s on a modified sampling knob, so each is omitted, observably.
        // Emitted before the `extra` overlay (the reader pulled these keys out of extra).
        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress {
                thinking: thinking_emitted,
            },
            &mut out,
        );
        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));
        // Controls with no Messages form (the penalties, `seed`, `n`, an unnameable tier, typed
        // slots with no member) are dropped with a warn and reported by `dropped_egress_controls`.
        crate::codec::carry::warn_drops(
            super::map::REQUEST,
            super::map::CONTROLS,
            Some(&super::map::DROP_WARN),
            req,
        );
        // The end-user identifier as `metadata.user_id`, emitted before the `extra` overlay so a
        // native `metadata` object in `extra` wins.
        if let Some(user) = &req.user {
            out.insert(
                keys::METADATA.to_string(),
                serde_json::json!({(super::USER_ID): user}),
            );
        }
        for (key, value) in &req.extra {
            // Skip busbar's positional-stash sentinel: `write_message` already consumed it, and
            // emitting it would put an undefined body key on the wire (a latent 400 and a proxy tell).
            if key == ANTHROPIC_UNMODELED_BLOCKS_SENTINEL {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }
        serde_json::Value::Object(out)
    }

    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        match ev {
            IrStreamEvent::MessageStart {
                role,
                usage,
                id,
                model,
                ..
            } => {
                let role_str = match role {
                    crate::codec::ir::IrRole::User => keys::USER,
                    crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                    _ => return None,
                };
                let mut msg_obj = serde_json::Map::new();
                // `message_start.message` is a skeleton Message an SDK reads
                // `id`/`type`/`role`/`model`/`content`/`usage` from (`stop_reason`/`stop_sequence`
                // null at start). Emit it unconditionally, synthesizing a `msg_` id when the source
                // carried none: this runs only on the cross-protocol path, where `StreamTranslate`
                // strips the foreign `id`, and a degenerate `{role,usage}` message_start fails SDK
                // decoding. The first `message_start` decides the stream's id; a duplicate replays it.
                let msg_id = self
                    .carried_message_id(|| id.clone().unwrap_or_else(|| synth_message_id(host)));
                msg_obj.insert(keys::ID.to_string(), serde_json::json!(msg_id));
                msg_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::MESSAGE));
                msg_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));
                // `model` is a required string in the SDK's type: emit it unconditionally (empty
                // when the source carried none) so the skeleton stays structurally valid.
                let model_str = model.as_deref().unwrap_or("");
                msg_obj.insert(keys::MODEL.to_string(), serde_json::json!(model_str));
                msg_obj.insert(
                    keys::CONTENT.to_string(),
                    serde_json::Value::Array(Vec::new()),
                );
                msg_obj.insert(super::STOP_REASON.to_string(), serde_json::Value::Null);
                msg_obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::Value::Null);
                // The published `Message` schema also requires nullable `stop_details` and
                // `container`, `null` at stream open.
                msg_obj.insert(super::STOP_DETAILS.to_string(), serde_json::Value::Null);
                msg_obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);
                // `usage` is a REQUIRED field of `message_start.message`, typed as the same full
                // `Usage` schema the buffered response carries: a client that reads
                // `event.message.usage.input_tokens` on the first event throws if it is absent. On
                // the cross-protocol path (e.g. OpenAI→Anthropic) the first chunk carries no usage,
                // so `usage` is `None`; `write_usage_object` emits the zero-valued skeleton in that
                // case (which also matches native behavior: output_tokens is 0 at stream open) and
                // fills every spec-required member either way.
                msg_obj.insert(keys::USAGE.to_string(), write_usage_object(usage.as_ref()));
                let mut data_obj = serde_json::Map::new();
                // Native SSE data bodies carry a top-level `type` matching the `event:` header, and
                // consumers that dispatch on `data.type` require it: emit it on every event body.
                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(EVT_MESSAGE_START));
                data_obj.insert(
                    keys::MESSAGE.to_string(),
                    serde_json::Value::Object(msg_obj),
                );
                Some((
                    EVT_MESSAGE_START.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::BlockStart {
                index,
                block,
                refusal: _,
            } => {
                let content_block = match block {
                    // `content_block_start` carries the block's seed so an SDK accumulator
                    // initializes the field before any delta: `text:""`, tool_use `input:{}`,
                    // thinking `thinking:""` + `signature:""`. Without it the first delta
                    // concatenates onto `undefined` and accumulation breaks. The response schemas
                    // also require `citations` on text (null; they stream as `citations_delta`)
                    // and `caller` on tool_use (`{"type":"direct"}`).
                    IrBlockMeta::Text => {
                        serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): "", (keys::CITATIONS): null })
                    }
                    IrBlockMeta::Thinking { .. } => {
                        serde_json::json!({ (keys::TYPE): keys::THINKING, (keys::THINKING): "", (keys::SIGNATURE): "" })
                    }
                    // A redacted thinking block emits no start here: native carries its opaque `data`
                    // inline on the start (no seed, no delta), so the start is written from the
                    // following `RedactedReasoningDelta`. A plaintext `thinking` seed here would
                    // mis-type the block and duplicate the start.
                    //
                    // The index is not marked open here: the open set is what the paired
                    // `BlockStop` consults, and a stream that ends before the delta (truncation, an
                    // upstream error, `close_open_blocks`) would otherwise close a block the client
                    // never saw opened. The delta that writes the start marks it open.
                    IrBlockMeta::RedactedThinking => return None,
                    IrBlockMeta::ToolUse { id, name } => {
                        serde_json::json!({
                            (keys::TYPE): STOP_TOOL_USE,
                            (keys::ID): id,
                            (keys::NAME): name,
                            (keys::INPUT): {},
                            (super::W_CALLER): { (keys::TYPE): super::DIRECT },
                        })
                    }
                    // An image block has no Anthropic response projection: the published
                    // `content_block` union has no `image` member, so an `image` frame is one no
                    // client can deserialize. Emit no frame and do not mark the index open, so the
                    // paired `BlockStop` stays silent.
                    IrBlockMeta::Image => return None,
                };
                self.mark_block_open(*index);
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_START),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                data_obj.insert(super::CONTENT_BLOCK.to_string(), content_block);
                Some((
                    EVT_CONTENT_BLOCK_START.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::BlockDelta { index, delta } => {
                let delta_val = match delta {
                    IrDelta::TextDelta(text) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_TEXT, (keys::TEXT): text })
                    }
                    IrDelta::ThinkingDelta(thinking) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_THINKING, (keys::THINKING): thinking })
                    }
                    IrDelta::InputJsonDelta(json) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_INPUT_JSON, (super::PARTIAL_JSON): json })
                    }
                    IrDelta::SignatureDelta(sig) => {
                        serde_json::json!({ (keys::TYPE): DELTA_TYPE_SIGNATURE, (keys::SIGNATURE): sig })
                    }
                    // A streamed redacted-reasoning delta (opaque encrypted bytes). There is no
                    // redacted delta type on the wire and the paired `BlockStart` was suppressed, so
                    // this delta emits the block's sole start, a `redacted_thinking`
                    // content_block_start carrying the bytes; `BlockStop` then closes it. This
                    // preserves the blob a later turn must replay for extended-thinking continuity.
                    IrDelta::MediaDelta(_) => return None,
                    IrDelta::RedactedReasoningDelta(bytes) => {
                        // Writing the start and marking the index open are one decision.
                        // `mark_block_open` reports whether the index was newly opened, so a second
                        // redacted delta adds no duplicate start.
                        if !self.mark_block_open(*index) {
                            return None;
                        }
                        let mut data_obj = serde_json::Map::new();
                        data_obj.insert(
                            keys::TYPE.to_string(),
                            serde_json::json!(EVT_CONTENT_BLOCK_START),
                        );
                        data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                        data_obj.insert(
                            super::CONTENT_BLOCK.to_string(),
                            serde_json::json!({
                                (keys::TYPE): BLOCK_TYPE_REDACTED_THINKING,
                                (keys::DATA): bytes,
                            }),
                        );
                        return Some((
                            EVT_CONTENT_BLOCK_START.to_string(),
                            serde_json::Value::Object(data_obj),
                        ));
                    }
                    // Anthropic has no logprobs: lossy by target, emit nothing.
                    IrDelta::LogprobsDelta(_) => return None,
                    // Streaming citation: each carried citation is its own `citations_delta` (the
                    // wire carries one `citation` per delta). `write_citation` re-emits a
                    // byte-exact Anthropic `raw` verbatim and otherwise synthesizes the object from
                    // neutral fields; `is_anthropic_citation_shape` keeps a foreign `raw` out.
                    IrDelta::CitationsDelta(citations) => {
                        // A `citations_delta` carries exactly one citation object, never an array
                        // (an SDK crashes on one). The writer returns at most one event per call, and
                        // `StreamTranslate` splits a multi-citation delta upstream, so each call sees
                        // one. An empty vec emits no event. A vec of >1 cannot occur via the framer;
                        // defensively take the first.
                        let c = citations.first()?;
                        let mut data_obj = serde_json::Map::new();
                        data_obj.insert(
                            keys::TYPE.to_string(),
                            serde_json::json!(EVT_CONTENT_BLOCK_DELTA),
                        );
                        data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                        data_obj.insert(
                            keys::DELTA.to_string(),
                            serde_json::json!({
                                (keys::TYPE): DELTA_TYPE_CITATIONS,
                                (keys::CITATION): write_citation(c),
                            }),
                        );
                        return Some((
                            EVT_CONTENT_BLOCK_DELTA.to_string(),
                            serde_json::Value::Object(data_obj),
                        ));
                    }
                };
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_DELTA),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                data_obj.insert(keys::DELTA.to_string(), delta_val);
                Some((
                    EVT_CONTENT_BLOCK_DELTA.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            // An untracked index is a block whose start had no projection (Image); closing it would
            // orphan a `content_block_stop`.
            IrStreamEvent::BlockStop { index } => {
                if !self.take_block_open(*index) {
                    return None;
                }
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(EVT_CONTENT_BLOCK_STOP),
                );
                data_obj.insert(keys::INDEX.to_string(), serde_json::json!(index));
                Some((
                    EVT_CONTENT_BLOCK_STOP.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::MessageDelta {
                stop_reason,
                stop_sequence,
                usage,
                stop_detail,
            } => {
                let mut delta_obj = serde_json::Map::new();
                if let Some(reason) = stop_reason {
                    delta_obj.insert(
                        super::STOP_REASON.to_string(),
                        serde_json::json!(write_anthropic_stop_reason_detailed(
                            *reason,
                            stop_detail.as_ref()
                        )),
                    );
                } else {
                    delta_obj.insert(super::STOP_REASON.to_string(), serde_json::Value::Null);
                }
                // `stop_sequence`: `message_delta` always carries it, the matched stop string or
                // explicit `null`.
                delta_obj.insert(
                    keys::STOP_SEQUENCE.to_string(),
                    stop_sequence
                        .as_deref()
                        .map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );
                // `MessageDelta` also requires nullable `stop_details` (the refusal object when the
                // IR carries one) and `container` (always `null`: busbar has no container).
                delta_obj.insert(
                    super::STOP_DETAILS.to_string(),
                    write_anthropic_stop_details(*stop_reason, stop_detail.as_ref()),
                );
                delta_obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);
                // `usage`: every required `MessageDeltaUsage` member, plus the 5m/1h tier split when
                // reported, so a request reconciles per tier at `stream: true` as at `stream: false`.
                let mut data_obj = serde_json::Map::new();
                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(EVT_MESSAGE_DELTA));
                data_obj.insert(
                    keys::DELTA.to_string(),
                    serde_json::Value::Object(delta_obj),
                );
                data_obj.insert(keys::USAGE.to_string(), write_message_delta_usage(usage));
                Some((
                    EVT_MESSAGE_DELTA.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
            IrStreamEvent::MessageStop => Some((
                EVT_MESSAGE_STOP.to_string(),
                serde_json::json!({ (keys::TYPE): EVT_MESSAGE_STOP }),
            )),
            IrStreamEvent::Error(err) => {
                // In-stream error event:
                // `{"type":"error","error":{"type":<type>,"message":<msg>},"request_id":..}`. The
                // SDK reads both `error.type` (typed exception) and `error.message`.
                //
                // `error.type` must be one of the nine published ErrorResponse tokens: a provider
                // signal that is one stays the type; any other (free text no SDK could map) becomes
                // the message and the type is the token for the error's class.
                let signal = err.provider_signal.as_deref().filter(|s| !s.is_empty());
                let (err_type, message) = match signal {
                    Some(ps) if is_error_type_token(ps) => (ps, ps.to_string()),
                    Some(ps) => (class_error_type(err.class), ps.to_string()),
                    None => (
                        class_error_type(err.class),
                        "an error occurred while streaming the response".to_string(),
                    ),
                };
                // The message text must stay native-plausible: no reverse-proxy vocabulary
                // ("upstream", "gateway", "backend"), so provider text is verbatim and the fallback
                // generic.
                let mut error_obj = serde_json::Map::new();
                error_obj.insert(keys::TYPE.to_string(), serde_json::json!(err_type));
                error_obj.insert(keys::MESSAGE.to_string(), serde_json::json!(message));
                let mut data_obj = serde_json::Map::new();
                // The body carries top-level `type:"error"` matching the `event: error` header; an
                // SDK dispatching on `data.type` will not recognize the error without it.
                data_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::ERROR_WORD));
                data_obj.insert(
                    keys::ERROR_WORD.to_string(),
                    serde_json::Value::Object(error_obj),
                );
                // The body is an ErrorResponse, whose schema requires a nullable top-level
                // `request_id`; the buffered `error_envelope` mints one and so does this event.
                data_obj.insert(
                    super::REQUEST_ID.to_string(),
                    serde_json::json!(synth_request_id(host)),
                );
                Some((
                    keys::ERROR_WORD.to_string(),
                    serde_json::Value::Object(data_obj),
                ))
            }
        }
    }

    fn write_error_frame(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        err: &IrError,
    ) -> Option<(String, serde_json::Value)> {
        // Delegate to the event writer so the mid-stream error frame is what an `Error` event produces.
        self.write_response_event(host, &IrStreamEvent::Error(err.clone()))
    }

    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();

        // id: `Message.id` is a required `msg_<rand>` string, so emit it unconditionally. A source
        // id is re-emitted verbatim; an absent one (including a Bedrock Converse body, which has
        // neither id nor created) is synthesized via `synth_message_id`. This is safe because
        // `write_response` runs only on the cross-protocol path; same-protocol non-stream relays
        // the raw body.
        let id = resp.id.clone().unwrap_or_else(|| synth_message_id(host));
        obj.insert(keys::ID.to_string(), serde_json::json!(id));

        // type/role are constant for a Messages API response ("message"/"assistant").
        obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::MESSAGE));
        obj.insert(keys::ROLE.to_string(), serde_json::json!(keys::ASSISTANT));

        // model: a required string in the SDK types, so emit it unconditionally; fall back to an
        // empty string when the reader did not surface one (notably Bedrock/Cohere).
        let model = resp.model.as_deref().unwrap_or("");
        obj.insert(keys::MODEL.to_string(), serde_json::json!(model));

        // content blocks in their response shape (see `write_response_block`).
        //
        // A response block is never an `image` or `document` and has no JSON block, so a foreign
        // backend's image or attachment output is omitted with a warn (as the streamed path does
        // for `IrBlockMeta::Image`) and a JSON block rides as the text of its JSON.
        let content_array: Vec<serde_json::Value> = resp
            .content
            .iter()
            .filter_map(|block| match block {
                crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. } => {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::IMAGE,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [],
                        "dropping image/attachment output block on Anthropic response egress: an \
                         Anthropic assistant message has no image or document response block"
                    );
                    None
                }
                crate::codec::ir::IrBlock::Json(v) => Some(serde_json::json!({
                    (keys::TYPE): keys::TEXT,
                    (keys::TEXT): serde_json::to_string(v).unwrap_or_default(),
                    (keys::CITATIONS): null,
                })),
                crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                    super::warn_hosted_record_dropped();
                    None
                }
                other => Some(write_response_block(other)),
            })
            .collect();
        obj.insert(
            keys::CONTENT.to_string(),
            serde_json::Value::Array(content_array),
        );

        // stop_reason: required and nullable; an explicit `null` when the source carried none.
        // `read_response` maps `null` back to `None`.
        obj.insert(
            super::STOP_REASON.to_string(),
            resp.stop_reason
                .map(|reason| {
                    serde_json::json!(write_anthropic_stop_reason_detailed(
                        reason,
                        resp.stop_detail.as_ref()
                    ))
                })
                .unwrap_or(serde_json::Value::Null),
        );

        // stop_sequence: a native `Message` always carries it, the matched stop string or `null`.
        // Emit an explicit `null` when absent; `read_response` maps it back to `None`.
        match &resp.stop_sequence {
            Some(seq) => {
                obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::json!(seq));
            }
            None => {
                obj.insert(keys::STOP_SEQUENCE.to_string(), serde_json::Value::Null);
            }
        }

        // stop_details / container: required, nullable. `stop_details` is the refusal object when
        // the IR carries one; `container` is always `null`.
        obj.insert(
            super::STOP_DETAILS.to_string(),
            write_anthropic_stop_details(resp.stop_reason, resp.stop_detail.as_ref()),
        );
        obj.insert(keys::CONTAINER.to_string(), serde_json::Value::Null);

        // usage: every member the published `Usage` schema requires, with the source's values
        // where it reported them and the spec's zero/null/default shape otherwise.
        obj.insert(
            keys::USAGE.to_string(),
            write_usage_object(Some(&resp.usage)),
        );

        serde_json::Value::Object(obj)
    }
}

/// The nine `error.type` tokens of Anthropic's published ErrorResponse.
fn is_error_type_token(s: &str) -> bool {
    [
        ERR_TYPE_API_ERROR,
        ERR_TYPE_AUTHENTICATION,
        ERR_TYPE_BILLING,
        ERR_TYPE_INVALID_REQUEST,
        ERR_TYPE_NOT_FOUND,
        ERR_TYPE_OVERLOADED,
        ERR_TYPE_PERMISSION,
        ERR_TYPE_RATE_LIMIT,
        ERR_TYPE_TIMEOUT,
    ]
    .contains(&s)
}

/// The Anthropic `error.type` for an error class, when the provider named no type of its own.
fn class_error_type(class: busbar_contract::upstream::StatusClass) -> &'static str {
    use busbar_contract::upstream::StatusClass as C;
    match class {
        C::RateLimit => ERR_TYPE_RATE_LIMIT,
        C::Overloaded => ERR_TYPE_OVERLOADED,
        C::Timeout => ERR_TYPE_TIMEOUT,
        C::Auth => ERR_TYPE_AUTHENTICATION,
        C::Billing => ERR_TYPE_BILLING,
        C::ClientError | C::ContextLength => ERR_TYPE_INVALID_REQUEST,
        C::ServerError | C::Network => ERR_TYPE_API_ERROR,
    }
}
