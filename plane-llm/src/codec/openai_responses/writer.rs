use super::*;

/// One Responses SSE frame: the event name, and its data, whose `type` IS the event name (spelled
/// once per frame here; `sequence_number` is stamped by `write_response_events`).
fn event(name: &'static str, mut data: serde_json::Value) -> (String, serde_json::Value) {
    if let Some(obj) = data.as_object_mut() {
        obj.insert(keys::TYPE.to_string(), serde_json::Value::from(name));
    }
    (name.to_string(), data)
}

impl ProtocolWriter for ResponsesWriter {
    fn upstream_path(&self) -> &str {
        "/v1/responses"
    }

    /// Latch the original ingress request body so `write_response_events` can answer the spec's
    /// request-echo members with the client's values (see `ResponsesWriter::store_request_echo`).
    fn set_request_echo(&self, ingress_request_body: &serde_json::Value) {
        self.store_request_echo(ingress_request_body);
    }

    /// Latch the stream's translation cap from the plane's settings.
    fn set_translate_cap(&self, cap: usize) {
        self.store_translate_cap(cap);
    }

    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        // Mirrors the `write_request` drop-warns: controls the mapping file has no row for (`top_k`,
        // `stop`, penalties, `seed`, `n`) are derived from it.
        let mut dropped: Vec<&'static str> =
            crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
                .map(crate::codec::carry::Slot::name)
                .collect();
        // IR-11: a hosted kind with no Responses tool (URL fetch).
        for tool in &req.hosted_tools {
            if matches!(tool, crate::codec::ir::IrHostedTool::WebFetch(_)) {
                dropped.push(tool.kind_str());
            }
        }
        // IR-19: Responses has no output-modality ask; a text-only ask loses nothing.
        if crate::codec::carry::Slot::OutputModalities.carried(req) {
            dropped.push("modalities");
        }
        dropped
    }

    /// `input` may be a list of items or a bare string (one implicit user turn); either is replaced
    /// by the EasyInputMessage list. Only an absent `input` is left untouched.
    fn apply_rewrite_to_ingress_body(
        &self,
        obj: &mut serde_json::Map<String, serde_json::Value>,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> bool {
        if obj.get(keys::INPUT).is_none() {
            return false;
        }
        let Some(pairs) = crate::codec::dialect::rewrite_text_pairs(messages) else {
            return false;
        };
        let framed: Vec<serde_json::Value> = pairs
            .into_iter()
            .map(|(role, text)| serde_json::json!({ (keys::ROLE): role, (keys::CONTENT): text }))
            .collect();
        obj.insert(keys::INPUT.to_string(), serde_json::Value::Array(framed));
        true
    }

    fn write_request(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
    ) -> serde_json::Value {
        self.write_request_for_lane(
            host,
            req,
            "",
            &busbar_contract::ir::egress_prep::LaneCaps::default(),
        )
    }

    fn write_request_for_lane(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &busbar_contract::ir::egress_prep::LaneCaps,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        let mut input_arr: Vec<serde_json::Value> = Vec::new();

        if !req.system.is_empty() {
            let instructions: String = req
                .system
                .iter()
                .filter_map(|block| match block {
                    crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            // IR-14: a `developer`-role system prompt goes back as a leading `developer` input item;
            // any other keeps the top-level `instructions`.
            if !instructions.is_empty() {
                if req.system_role == Some(crate::codec::ir::IrSystemRole::Developer) {
                    input_arr.push(serde_json::json!({
                        (keys::ROLE): crate::codec::ir::IrSystemRole::Developer.as_str(),
                        (keys::CONTENT): instructions
                    }));
                } else {
                    out.insert(
                        keys::INSTRUCTIONS.to_string(),
                        serde_json::json!(instructions),
                    );
                }
            }
        }

        for msg in &req.messages {
            match msg.role {
                crate::codec::ir::IrRole::User | crate::codec::ir::IrRole::Assistant => {
                    let role_str = if msg.role == crate::codec::ir::IrRole::User {
                        keys::USER
                    } else {
                        keys::ASSISTANT
                    };

                    let mut content_arr: Vec<serde_json::Value> = Vec::new();
                    // `function_call` / `function_call_output` items are flat top-level `input`
                    // entries, not nested in a message's `content`. Collected separately so the
                    // assistant `message` comes first (only if it has content), tool items after it.
                    let mut tool_items: Vec<serde_json::Value> = Vec::new();
                    // Prior-turn reasoning becomes top-level `reasoning` input items before their message.
                    let mut reasoning_items: Vec<serde_json::Value> = Vec::new();
                    for block in &msg.content {
                        match block {
                            // IR-02: an assistant refusal goes back as the Responses `refusal` part.
                            crate::codec::ir::IrBlock::Text {
                                text,
                                refusal: true,
                                ..
                            } if msg.role == crate::codec::ir::IrRole::Assistant => {
                                content_arr.push(serde_json::json!({
                                    (keys::TYPE): keys::REFUSAL,
                                    (keys::REFUSAL): text
                                }));
                            }
                            crate::codec::ir::IrBlock::Text {
                                text, citations, ..
                            } => {
                                let type_str = if msg.role == crate::codec::ir::IrRole::User {
                                    CONTENT_TYPE_INPUT_TEXT
                                } else {
                                    CONTENT_TYPE_OUTPUT_TEXT
                                };
                                let mut part = serde_json::Map::new();
                                part.insert(keys::TYPE.to_string(), serde_json::json!(type_str));
                                part.insert(keys::TEXT.to_string(), serde_json::json!(text));
                                // Re-emit URL-citation `annotations` on an assistant `output_text`
                                // part, only when non-empty (`input_text` carries none).
                                if msg.role == crate::codec::ir::IrRole::Assistant
                                    && !citations.is_empty()
                                {
                                    let annotations =
                                        super::super::url_citation_wire::url_annotations(
                                            text, 0, citations,
                                        );
                                    if !annotations.is_empty() {
                                        part.insert(
                                            keys::ANNOTATIONS.to_string(),
                                            serde_json::Value::Array(annotations),
                                        );
                                    }
                                }
                                content_arr.push(serde_json::Value::Object(part));
                            }
                            crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                                content_arr.extend(input_image_part(source, *detail));
                            }
                            // The input surface has one attachment part, `input_file`, and no audio
                            // or video part: a document projects natively, other kinds are dropped
                            // with a warn naming the construct, never as an empty text part.
                            crate::codec::ir::IrBlock::Media {
                                kind, source, name, ..
                            } => {
                                content_arr.extend(input_file_part(*kind, source, name.as_deref()));
                            }
                            crate::codec::ir::IrBlock::Json(_)
                            | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                                // Structured JSON (Bedrock tool-result content) has no input-content shape.
                            }
                            crate::codec::ir::IrBlock::ToolUse {
                                id, name, input, ..
                            } => {
                                // A raw `Value::String` (unparseable or partial args) goes verbatim,
                                // not JSON-encoded twice, as in the Chat writer.
                                let args_str =
                                    crate::codec::dialect::tool_arguments_to_string(input);
                                tool_items.push(serde_json::json!({
                                    (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                                    (CALL_ID): id,
                                    (keys::NAME): name,
                                    (keys::ARGUMENTS): args_str
                                }));
                            }
                            crate::codec::ir::IrBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                // RSP-10: text, JSON, images and files all reach `output`
                                // (`function_call_output_value`).
                                let output_value = function_call_output_value(content);

                                tool_items.push(serde_json::json!({
                                    (keys::TYPE): FUNCTION_CALL_OUTPUT,
                                    (CALL_ID): tool_use_id,
                                    (keys::OUTPUT): output_value
                                }));
                            }
                            // A prior-turn Thinking block re-emits as a top-level `reasoning` INPUT
                            // item (a sibling of the message), mirroring `write_response`. A
                            // redacted block has no plaintext analog here and is dropped.
                            // ROLE GUARD: the item asserts "my own prior reasoning", so only an
                            // Assistant message may carry it. A Thinking block on a User message
                            // (e.g. Gemini `thought: true` parts) would present caller content to
                            // the model with false provenance.
                            crate::codec::ir::IrBlock::Thinking {
                                text,
                                signature,
                                redacted,
                                kind,
                                signature_origin,
                                ..
                            } if !*redacted && msg.role == crate::codec::ir::IrRole::Assistant => {
                                // RSP-13 (IR-18): only an OpenAI-minted blob (or unknown origin) is
                                // sent as `encrypted_content`; a foreign signature cannot be decrypted.
                                let emit_sig = signature
                                    .as_deref()
                                    .filter(|_| super::slots::own_signature(*signature_origin));
                                if signature.is_some() && emit_sig.is_none() {
                                    crate::codec::drops::writer_drop!(
                                        crate::codec::drops::THINKING,
                                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                        [origin = ?signature_origin, ],
                                        "dropping a foreign reasoning signature on Responses egress: \
                                         `encrypted_content` accepts only an OpenAI-minted blob");
                                }
                                // A wholly empty reasoning block emits no item.
                                if !text.is_empty() || emit_sig.is_some() {
                                    let mut item = serde_json::Map::new();
                                    item.insert(
                                        keys::TYPE.to_string(),
                                        serde_json::json!(ITEM_TYPE_REASONING),
                                    );
                                    item.insert(
                                        keys::ID.to_string(),
                                        serde_json::json!(synthesize_item_id(
                                            host,
                                            ITEM_ID_PREFIX_RS
                                        )),
                                    );
                                    // IR-17: a summary goes back into `summary[]`.
                                    super::slots::insert_reasoning_text(&mut item, text, *kind);
                                    if let Some(sig) = emit_sig {
                                        item.insert(
                                            ENCRYPTED_CONTENT.to_string(),
                                            serde_json::json!(sig),
                                        );
                                    }
                                    reasoning_items.push(serde_json::Value::Object(item));
                                }
                            }
                            // Non-Assistant Thinking (role guard above): drop with a warn, as for
                            // the other arms with no analog on this surface.
                            crate::codec::ir::IrBlock::Thinking { redacted, .. } if !*redacted => {
                                crate::codec::drops::writer_drop!(
                                    crate::codec::drops::THINKING,
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [],
                                    "dropping non-Assistant Thinking block on Responses egress: a \
                                     `reasoning` input item asserts it is the model's own prior \
                                     reasoning, which only an Assistant-role message can carry"
                                );
                            }
                            // A redacted block (Anthropic `redacted_thinking`, Bedrock
                            // `redactedContent`) has no Responses form: `encrypted_content` takes
                            // only an OpenAI blob (RSP-13). Dropped, not leaked.
                            crate::codec::ir::IrBlock::Thinking { .. } => {}
                        }
                    }

                    input_arr.extend(reasoning_items);

                    // Emit the `message` wrapper only with content: a pure tool-call turn must not
                    // produce `{role, content: []}`, which the API rejects.
                    if !content_arr.is_empty() {
                        let mut msg_obj = serde_json::Map::new();
                        msg_obj.insert(keys::ROLE.to_string(), serde_json::json!(role_str));
                        msg_obj.insert(
                            keys::CONTENT.to_string(),
                            serde_json::Value::Array(content_arr),
                        );
                        input_arr.push(serde_json::Value::Object(msg_obj));
                    }
                    input_arr.extend(tool_items);
                }

                crate::codec::ir::IrRole::Tool => {
                    for block in &msg.content {
                        if let crate::codec::ir::IrBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = block
                        {
                            let output_value = function_call_output_value(content);

                            input_arr.push(serde_json::json!({
                                (keys::TYPE): FUNCTION_CALL_OUTPUT,
                                (CALL_ID): tool_use_id,
                                (keys::OUTPUT): output_value
                            }));
                        }
                    }
                }

                crate::codec::ir::IrRole::System => {}
            }
        }

        if !input_arr.is_empty() {
            out.insert(keys::INPUT.to_string(), serde_json::Value::Array(input_arr));
        }

        // IR-11: neutral hosted tools in Responses spelling (a kind with none is dropped with a warn
        // and reported by `dropped_egress_controls`).
        let hosted_arr: Vec<serde_json::Value> = req
            .hosted_tools
            .iter()
            .filter_map(super::slots::write_hosted_tool)
            .collect();
        let has_tools = !req.tools.is_empty() || !hosted_arr.is_empty();
        if has_tools {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in &req.tools {
                // Hosted-tool passthrough: a hosted tool is a complete spec keyed by its top-level
                // `type` with no `name`/`parameters`, so re-emit the raw JSON verbatim. Wrapping it as
                // a function tool gives `{"type":"function","name":""}`, which the backend 400s.
                if let Some(hosted) = &tool.hosted {
                    tools_arr.push(hosted.clone());
                    continue;
                }
                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(keys::TYPE.to_string(), serde_json::json!(keys::FUNCTION));
                tool_obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));

                if let Some(desc) = &tool.description {
                    tool_obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }

                let params = if !tool.input_schema.is_null() {
                    tool.input_schema.clone()
                } else {
                    serde_json::json!({})
                };
                tool_obj.insert(keys::PARAMETERS.to_string(), params);

                // Flat on a Responses tool (Chat nests it). Only when stated, so `None` never
                // becomes `strict: false`.
                if let Some(strict) = tool.strict {
                    tool_obj.insert(keys::STRICT.to_string(), serde_json::json!(strict));
                }

                tools_arr.push(serde_json::Value::Object(tool_obj));
            }
            tools_arr.extend(hosted_arr);
            out.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
        }

        // Emit `tool_choice` natively so a forced directive from another protocol does not degrade
        // to `auto`. `/v1/responses` rejects it tool-less, as Chat does; the reachable case is a
        // cross-protocol request whose hosted tools `prepare_for_egress` stripped
        // (`ir/variant.rs`). IR-10 (RSP-15): a restricted subset is the `allowed_tools` form, its
        // mode from the Auto/Required directive beside it.
        if let Some(names) = &req.allowed_tools {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping allowed_tools tool_choice on Responses egress: tool_choice is only \
                     allowed when tools are specified"
                );
            } else {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    super::slots::write_allowed_tools(names, req.tool_choice.as_ref()),
                );
            }
        } else if let Some(tc) = &req.tool_choice {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on Responses egress: tool_choice is only allowed when \
                     tools are specified (likely because the hosted tools that carried it were \
                     stripped on the cross-protocol seam)"
                );
            } else {
                out.insert(
                    keys::TOOL_CHOICE.to_string(),
                    write_responses_tool_choice(tc),
                );
            }
        }
        // `parallel_tool_calls` is meaningless (and rejected) with no tools, as on Chat. The
        // `is_some()` gate keeps it from firing on tool-less requests.
        if let Some(parallel) = req.parallel_tool_calls {
            if !has_tools {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::PARALLEL_TOOL_CALLS,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping parallel_tool_calls on Responses egress: it has no accompanying \
                     tools (likely because the hosted tools that carried it were stripped on the \
                     cross-protocol seam), so the backend's default parallelism applies"
                );
            } else {
                out.insert(
                    keys::PARALLEL_TOOL_CALLS.to_string(),
                    serde_json::json!(parallel),
                );
            }
        }

        if let Some(max_tokens) = req.max_tokens {
            out.insert(
                INCOMPLETE_REASON_MAX_OUTPUT.to_string(),
                serde_json::json!(max_tokens),
            );
        }

        // `temperature` / `top_p` are map-file rows (written below); top_k/stop have no target.

        // Logprobs ask: a top-level `top_logprobs` (0-20). There is no `logprobs` boolean on
        // `/v1/responses`; response-side logprobs ride `include`.
        if let Some(top_logprobs) = req.top_logprobs {
            out.insert(
                keys::TOP_LOGPROBS.to_string(),
                serde_json::json!(top_logprobs),
            );
        }

        // RSP-06: the end-user id rides `user`, as on Chat.
        if let Some(user) = &req.user {
            out.insert(keys::USER.to_string(), serde_json::json!(user));
        }

        if crate::codec::carry::Slot::OutputModalities.carried(req) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::OUTPUT_MODALITIES,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "responses writer: /v1/responses models no output-modality ask; dropping the \
                 non-text modalities (lossy-by-target)"
            );
        }

        // Controls the create API does not model (`top_k`, `stop`, penalties, `seed`, `n`) would 400
        // a real call, so each is dropped observably via the mapping file's words and
        // `dropped_egress_controls`.
        crate::codec::carry::warn_drops(super::map::REQUEST, super::map::CONTROLS, None, req);

        // response_format goes under `text.format` (flat json_schema), not a top-level member. It is
        // merged into any `text` object forwarded via `extra` (e.g. `verbosity`) so both survive;
        // `extra` is applied first (below) so the merge sees it.
        if let Some(rf) = &req.response_format {
            let format = write_text_format(rf);
            // Start from the `text` remainder the reader preserved, so `verbosity` etc. survive.
            let mut text_obj = req
                .extra
                .get(keys::TEXT)
                .and_then(|t| t.as_object())
                .cloned()
                .unwrap_or_default();
            text_obj.insert(keys::FORMAT.to_string(), format);
            out.insert(keys::TEXT.to_string(), serde_json::Value::Object(text_obj));
            // The extra-forwarding loop below skips `text` when `response_format` is Some.
        }
        // Flat request fields (`map.gen.rs`): IR-03..06 and IR-07 `text.verbosity`, overlaid on the
        // `text` above. A same-protocol request's raw members in `extra` (below) win.
        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );

        // `stream` is a modeled key excluded from `extra`; omitting it would answer a `stream: true`
        // request non-streaming and stall the SSE loop.
        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));

        // Reasoning as `reasoning: {effort}`; a numeric budget is bucketized through the effort
        // table. Emitted only when `extra` carries no native `reasoning` object (the overlay below
        // wins, and can carry `summary`).
        //
        // `Off` (IR-09) is matched first: through the table it would read as the smallest enable
        // ask. `effort: "none"` is only accepted by the newest reasoning models, so it is sent only
        // for a lane declaring `LaneCaps::reasoning_none`, else omitted with a warn.
        if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) {
            if !req.extra.contains_key(keys::REASONING) {
                if caps.reasoning_none {
                    out.insert(
                        keys::REASONING.to_string(),
                        serde_json::json!({(keys::EFFORT): keys::NONE_WORD}),
                    );
                } else {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [],
                        "omitting reasoning OFF on Responses egress: reasoning.effort \"none\" is \
                         not accepted by every OpenAI reasoning model and this lane does not \
                         declare it"
                    );
                }
            }
        } else if let Some(ask) = req.reasoning {
            if !req.extra.contains_key(keys::REASONING) {
                let table = req
                    .reasoning_budgets
                    .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
                match ask.to_effort(table) {
                    Some(effort) => {
                        out.insert(
                            keys::REASONING.to_string(),
                            serde_json::json!({(keys::EFFORT): effort.as_three_word_str()}),
                        );
                    }
                    // "The model decides" has no effort word: dropped, never substituted (design F3).
                    None => crate::codec::drops::writer_drop!(
                        crate::codec::drops::REASONING,
                        &crate::codec::diagnostics::IR_DROP_REASONING,
                        [],
                        "dropping a \"model decides\" reasoning ask on Responses egress: \
                         reasoning.effort has no dynamic form"
                    ),
                }
            }
        }

        for (key, value) in &req.extra {
            // `text` in extra holds only the non-`format` remainder. When the IR has a
            // `response_format` the merged `text` was inserted above and must not be clobbered.
            if key == keys::TEXT && req.response_format.is_some() {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        // RSP-07: `top_logprobs` alone does not return logprobs; `include` must name
        // `message.output_text.logprobs`. Added after the `extra` overlay so the caller's `include`
        // entries are kept without a duplicate.
        if req.logprobs == Some(true) {
            let include = out
                .entry(INCLUDE.to_string())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let Some(arr) = include.as_array_mut() {
                if !arr
                    .iter()
                    .any(|v| v.as_str() == Some(INCLUDE_OUTPUT_TEXT_LOGPROBS))
                {
                    arr.push(serde_json::json!(INCLUDE_OUTPUT_TEXT_LOGPROBS));
                }
            }
        }

        serde_json::Value::Object(out)
    }

    fn write_response_event(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Option<(String, serde_json::Value)> {
        // Single-frame contract. The streaming seam uses `write_response_events`; this is reached
        // only for events that frame to one wire event (e.g. `write_error_frame`) and by tests. A
        // multi-frame event yields only its first frame.
        self.write_response_events(host, ev).into_iter().next()
    }

    fn write_response_events(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        ev: &IrStreamEvent,
    ) -> Vec<(String, serde_json::Value)> {
        // The opening event resets the per-stream `sequence_number`. Only the first `MessageStart`
        // is that event (the Anthropic reader emits one per upstream frame), so a duplicate
        // continues the stream. The same latch decides the `response.id` below.
        let opening = matches!(ev, IrStreamEvent::MessageStart { .. })
            && !self.started.swap(true, Ordering::Relaxed);
        if opening {
            self.reset_sequence_number();
        }

        // Build the ordered wire frames. Most events yield 0 or 1; a text `BlockStart`/`BlockStop`
        // yields the multi-frame content-part bracket a strict SDK requires
        // (`output_item.added`, `content_part.added`; `output_text.done`, `content_part.done`,
        // `output_item.done`). Each frame is numbered below.
        let frames: Vec<(String, serde_json::Value)> = match ev {
            IrStreamEvent::MessageStart {
                id,
                created,
                model,
                usage,
                ..
            } => {
                // The SDK reads `response.id`/`created_at`/`model` from `response.created`. Forward
                // the captured identity (same-protocol), else synthesize a `resp_` id and the current
                // time (cross-protocol, where `translate_event` strips them).
                let mut resp_obj = serde_json::Map::new();
                // A native stream never changes its id, even across two `response.created`s: only the
                // first `MessageStart` decides it, so a duplicate restates the id. (The reset does not
                // clear the id cell, so a reused writer takes its new id from its own opening event.)
                let id = self
                    .carried_response_id()
                    .filter(|_| !opening)
                    .unwrap_or_else(|| id.clone().unwrap_or_else(|| synthesize_response_id(host)));
                // Carry the id forward so terminal events replay it.
                self.set_response_id(&id);
                let created_at = created.unwrap_or_else(|| self.default_created_at(host));
                // Carry `created_at` forward; it is constant across a native stream.
                self.set_created_at(created_at);
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
                resp_obj.insert(keys::CREATED_AT.to_string(), serde_json::json!(created_at));
                resp_obj.insert(
                    keys::STATUS.to_string(),
                    serde_json::json!(STATUS_IN_PROGRESS),
                );
                // `Response.model` is required: emit it with the DEFAULT_MODEL fallback when the IR
                // carries none (omission breaks strict decoders).
                let model_name = model.as_deref().unwrap_or(DEFAULT_MODEL);
                // Carry the model forward; it is constant across a native stream.
                self.set_model(model_name);
                resp_obj.insert(keys::MODEL.to_string(), serde_json::json!(model_name));
                // `response.created` carries the full Response skeleton, which the SDK reads
                // unconditionally: empty `output`, `error: null`, and a zeroed `usage` (the opening
                // event's own count when the source carried one). `usage` is a non-nullable object.
                resp_obj.insert(keys::OUTPUT.to_string(), serde_json::json!([]));
                resp_obj.insert(keys::ERROR_WORD.to_string(), serde_json::Value::Null);
                resp_obj.insert(
                    keys::USAGE.to_string(),
                    usage
                        .as_ref()
                        .map(build_responses_usage)
                        .unwrap_or_else(zero_responses_usage),
                );
                // The request-echo members and nullable `incomplete_details` are required on every
                // Response object, including this skeleton.
                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());
                vec![event(
                    EVT_RESPONSE_CREATED,
                    serde_json::json!({ (keys::RESPONSE): resp_obj }),
                )]
            }

            IrStreamEvent::BlockStart {
                index,
                block,
                refusal,
            } => match block {
                crate::codec::ir::IrBlockMeta::Text => {
                    // A text part sits in a `message` item: `output_item.added(message)`,
                    // `content_part.added`, `output_text.delta`s, then `output_text.done`,
                    // `content_part.done`, `output_item.done`. A strict client (Codex CLI) drops
                    // deltas with no active content part, so `content_part.added` is load-bearing.
                    // This BlockStart emits both opening frames; BlockStop closes the part, then the
                    // item. The open index is tracked (capped) so BlockStop closes this index only.
                    if !self.open_text_item(*index) {
                        return Vec::new();
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index);
                    // IR-02: a refusal block opens a `refusal` part; its deltas and closes follow (`is_refusal`).
                    if *refusal {
                        self.mark_refusal(*index);
                    }
                    let part = if *refusal {
                        serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): "" })
                    } else {
                        serde_json::json!({
                            (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                            (keys::TEXT): "",
                            (keys::ANNOTATIONS): [],
                            (keys::LOGPROBS): []
                        })
                    };
                    vec![
                        event(
                            EVT_OUTPUT_ITEM_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): {
                                    (keys::TYPE): ITEM_TYPE_MESSAGE,
                                    (keys::ID): item_id,
                                    (keys::ROLE): keys::ASSISTANT,
                                    (keys::STATUS): STATUS_IN_PROGRESS,
                                    (keys::CONTENT): []
                                }
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                // Every frame of the item uses the single part's `content_index: 0`.
                                (CONTENT_INDEX): 0,
                                // Opens an empty `output_text` part (text arrives via deltas), in the
                                // closing part's shape: `type`/`text`/`annotations`/`logprobs`, all required.
                                (PART): part
                            }),
                        ),
                    ]
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    // `item_id` (`fc_…`) rides the `output_item.added`/`.done` pair; deterministic
                    // from the output index so the `.done`, which sees only the index, rebuilds it.
                    // Opening the index lets `BlockStop` emit `done` for this index only. A refused
                    // open (cap reached, index already open) emits no frame, as the text and reasoning
                    // arms do: an `added` past the cap would never be closed and would vanish from
                    // the terminal `output[]`.
                    if !self.mark_tool_open(*index) {
                        return Vec::new();
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_FC, *index);
                    // Capture call_id/name now: the native `done` carries them, `BlockStop` only the index.
                    self.record_tool_meta(*index, id, name);
                    vec![event(
                        EVT_OUTPUT_ITEM_ADDED,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): item_id,
                            // `arguments` is required on `FunctionToolCall`. Real OpenAI opens the
                            // item with `"arguments": ""` and fills it via the delta events; omitting
                            // it fails the schema and makes an SDK concatenate onto `undefined`.
                            (ITEM): {
                                (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                                (keys::ID): item_id,
                                (CALL_ID): id,
                                (keys::NAME): name,
                                (keys::ARGUMENTS): ""
                            }
                        }),
                    )]
                }
                // A redacted thinking block has no Responses output shape (redacted reasoning is
                // dropped on both paths). No item is opened, so the following
                // `RedactedReasoningDelta` and the `BlockStop` emit nothing: an event-balanced drop.
                crate::codec::ir::IrBlockMeta::RedactedThinking => Vec::new(),
                crate::codec::ir::IrBlockMeta::Thinking { kind } => {
                    // Open a native `reasoning` output item: `output_item.added` typed "reasoning"
                    // with a stable `rs_…` item_id, and track the index so BlockStop closes it.
                    if !self.open_reasoning_item(*index) {
                        return Vec::new();
                    }
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_RS, *index);
                    // IR-17: a summary block opens with its one `summary_text` part (no `content[]`).
                    if *kind == Some(crate::codec::ir::IrThinkingKind::Summary) {
                        self.mark_summary_reasoning(*index);
                        vec![
                            event(
                                EVT_OUTPUT_ITEM_ADDED,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): {
                                        (keys::TYPE): ITEM_TYPE_REASONING,
                                        (keys::ID): item_id,
                                        (SUMMARY): []
                                    }
                                }),
                            ),
                            event(
                                EVT_REASONING_SUMMARY_PART_ADDED,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (PART): { (keys::TYPE): SUMMARY_TEXT, (keys::TEXT): "" }
                                }),
                            ),
                        ]
                    } else {
                        vec![event(
                            EVT_OUTPUT_ITEM_ADDED,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): {
                                    (keys::TYPE): ITEM_TYPE_REASONING,
                                    (keys::ID): item_id,
                                    (SUMMARY): [],
                                    (keys::CONTENT): []
                                }
                            }),
                        )]
                    }
                }
                crate::codec::ir::IrBlockMeta::Image => Vec::new(),
            },

            IrStreamEvent::BlockDelta { index, delta } => match delta {
                // IR-02: text of a refusal block streams as `refusal.delta`.
                crate::codec::ir::IrDelta::TextDelta(text)
                    if !text.is_empty() && self.is_refusal(*index) =>
                {
                    self.append_text(*index, text);
                    vec![event(
                        EVT_REFUSAL_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index),
                            (CONTENT_INDEX): 0,
                            (keys::DELTA): text
                        }),
                    )]
                }
                crate::codec::ir::IrDelta::TextDelta(text) if !text.is_empty() => {
                    // `output_text.delta` carries `item_id` (the `msg_…` item, deterministic from the
                    // index) and `content_index: 0`. The fragment is accumulated so BlockStop can
                    // assemble the complete text for the terminal `output`.
                    self.append_text(*index, text);
                    // `logprobs` is required on every `output_text.delta`. The IR delivers them as a
                    // separate `LogprobsDelta` (buffered), so a text delta carries `[]`.
                    vec![event(
                        EVT_OUTPUT_TEXT_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index),
                            (CONTENT_INDEX): 0,
                            (keys::DELTA): text,
                            (keys::LOGPROBS): []
                        }),
                    )]
                }
                crate::codec::ir::IrDelta::InputJsonDelta(json_str) => {
                    // Accumulate so `output_item.done` emits the complete arguments string.
                    self.append_tool_arguments(*index, json_str);
                    vec![event(
                        EVT_FUNCTION_CALL_ARGS_DELTA,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_FC, *index),
                            (keys::DELTA): json_str
                        }),
                    )]
                }
                &crate::codec::ir::IrDelta::TextDelta(_) => Vec::new(),
                crate::codec::ir::IrDelta::ThinkingDelta(text) if !text.is_empty() => {
                    // `response.reasoning_text.delta` for the item at this index, accumulated for
                    // BlockStop. `content_index: 0`.
                    self.append_reasoning(*index, text);
                    let (event, part_key) = if self.is_summary_reasoning(*index) {
                        (EVT_REASONING_SUMMARY_TEXT_DELTA, SUMMARY_INDEX)
                    } else {
                        (EVT_REASONING_TEXT_DELTA, CONTENT_INDEX)
                    };
                    vec![(
                        event.to_string(),
                        serde_json::json!({
                            (keys::TYPE): event,
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): self.item_id_for(host, ITEM_ID_PREFIX_RS, *index),
                            part_key: 0,
                            (keys::DELTA): text
                        }),
                    )]
                }
                // RSP-01: a `SignatureDelta` has no Responses delta frame. It is buffered and lands
                // as `encrypted_content` on the reasoning item's `output_item.done` and terminal
                // `output[]`, as the buffered `write_response` does. Dropping it broke multi-turn
                // reasoning continuity.
                crate::codec::ir::IrDelta::SignatureDelta(sig) => {
                    self.append_reasoning_signature(*index, sig);
                    Vec::new()
                }
                // An empty ThinkingDelta carries nothing. Redacted reasoning is dropped on both paths,
                // so the stream matches the buffered body.
                &crate::codec::ir::IrDelta::ThinkingDelta(_)
                | crate::codec::ir::IrDelta::RedactedReasoningDelta(_) => Vec::new(),
                // IR-21: no Responses stream event carries a generated media part.
                crate::codec::ir::IrDelta::MediaDelta(_) => Vec::new(),
                // Citations ride as `annotations` on the assembled `output_text` part, not a delta
                // frame; nothing is emitted here, but they are buffered for `BlockStop`.
                crate::codec::ir::IrDelta::CitationsDelta(cits) => {
                    self.append_citations(*index, cits);
                    Vec::new()
                }
                // Logprobs ride `output_text.done` and the finalized part (built at `BlockStop`), not a
                // standalone frame; buffered here.
                crate::codec::ir::IrDelta::LogprobsDelta(lps) => {
                    self.append_logprobs(*index, lps);
                    Vec::new()
                }
            },

            IrStreamEvent::BlockStop { index } => {
                // `BlockStop` carries only the index. A native stream emits `output_item.done` only
                // for an item it `added`, and the type must match, so consult the per-stream open
                // sets: function-call closes typed "function_call", text typed "message", reasoning
                // typed "reasoning", and a never-opened index emits nothing. Mistyping (the old
                // hardcoded `function_call`) or an unmatched `done` breaks typed SDKs.
                //
                // Each branch yields its `Vec` as the match value, never `return`s, so the caller
                // numbers every frame with `sequence_number`. Text and reasoning closes yield several
                // ordered frames (the part's `.done` bracket, then the item's `output_item.done`).
                if self.take_reasoning_open(*index) {
                    // Close the reasoning item: `reasoning_text.done` closes the streamed part, then
                    // `output_item.done` typed "reasoning" with the same `rs_…` id and the assembled
                    // text under `content[]`. The finalized item is recorded for the terminal `output[]`.
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_RS, *index);
                    let text = self.take_reasoning_accum(*index);
                    let summary = self.take_summary_reasoning(*index);
                    // IR-17: the same `summary[]` / `content[]` placement as the buffered item.
                    let mut item_obj = serde_json::Map::new();
                    item_obj.insert(
                        keys::TYPE.to_string(),
                        serde_json::json!(ITEM_TYPE_REASONING),
                    );
                    item_obj.insert(keys::ID.to_string(), serde_json::json!(item_id));
                    super::slots::insert_reasoning_text(
                        &mut item_obj,
                        &text,
                        summary.then_some(crate::codec::ir::IrThinkingKind::Summary),
                    );
                    let mut item = serde_json::Value::Object(item_obj);
                    // RSP-01: the buffered signature becomes `encrypted_content`, as in `write_response`.
                    if let Some(sig) = self.take_reasoning_signature(*index) {
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert(ENCRYPTED_CONTENT.to_string(), serde_json::json!(sig));
                        }
                    }
                    self.record_output_item(*index, item.clone());
                    if summary {
                        vec![
                            event(
                                EVT_REASONING_SUMMARY_TEXT_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (keys::TEXT): text,
                                }),
                            ),
                            event(
                                EVT_REASONING_SUMMARY_PART_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (SUMMARY_INDEX): 0,
                                    (PART): { (keys::TYPE): SUMMARY_TEXT, (keys::TEXT): text },
                                }),
                            ),
                            event(
                                EVT_OUTPUT_ITEM_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): item,
                                }),
                            ),
                        ]
                    } else {
                        vec![
                            event(
                                EVT_REASONING_TEXT_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (CONTENT_INDEX): 0,
                                    (keys::TEXT): text,
                                }),
                            ),
                            event(
                                EVT_OUTPUT_ITEM_DONE,
                                serde_json::json!({
                                    (OUTPUT_INDEX): index,
                                    (ITEM_ID): item_id,
                                    (ITEM): item,
                                }),
                            ),
                        ]
                    }
                } else if self.take_tool_open(*index) {
                    // `output_item.done` carries the same `item_id` as the `added` (`item_id_for("fc",
                    // index)` is cached) plus the finalized `item`: `call_id`/`name` captured on `added`,
                    // `arguments` concatenated from the deltas. A poisoned-lock-empty accumulator
                    // degrades to empty-string fields.
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_FC, *index);
                    let accum = self.take_tool_accum(*index).unwrap_or_default();
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                        (keys::ID): item_id,
                        (CALL_ID): accum.call_id,
                        (keys::NAME): accum.name,
                        (keys::ARGUMENTS): accum.arguments,
                    });
                    // Record the finalized item for the terminal `output[]`.
                    self.record_output_item(*index, item.clone());
                    vec![event(
                        EVT_OUTPUT_ITEM_DONE,
                        serde_json::json!({
                            (OUTPUT_INDEX): index,
                            (ITEM_ID): item_id,
                            (ITEM): item,
                        }),
                    )]
                } else if self.is_refusal(*index) && self.take_text_open(*index) {
                    // IR-02: close a refusal item (`refusal.done`, `content_part.done`,
                    // `output_item.done`). A refusal part carries no annotations or logprobs, so any
                    // buffered are discarded.
                    self.take_refusal(*index);
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index);
                    let text = self.take_text_accum(*index);
                    let _ = self.take_citation_accum(*index);
                    let _ = self.take_logprob_accum(*index);
                    let part =
                        serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text });
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): item_id,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [part.clone()]
                    });
                    self.record_output_item(*index, item.clone());
                    vec![
                        event(
                            EVT_REFUSAL_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (keys::REFUSAL): text,
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (PART): part,
                            }),
                        ),
                        event(
                            EVT_OUTPUT_ITEM_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): item,
                            }),
                        ),
                    ]
                } else if self.take_text_open(*index) {
                    // Close the message item: `output_text.done` (assembled text), `content_part.done`
                    // (finalized part), `output_item.done`, with the cached `msg_…` id used on the
                    // deltas. A strict client (Codex CLI) needs the brackets to accept the text.
                    let item_id = self.item_id_for(host, ITEM_ID_PREFIX_MSG, *index);
                    let text = self.take_text_accum(*index);
                    let annotations = super::super::url_citation_wire::url_annotations(
                        &text,
                        0,
                        &self.take_citation_accum(*index),
                    );
                    // Buffered logprobs ride `output_text.done` and the finalized part; the spec
                    // requires the member on both, so none yields `[]`.
                    let logprobs = self.take_logprob_accum(*index);
                    let part_logprobs = write_responses_part_logprobs(&logprobs);
                    let event_logprobs = write_responses_event_logprobs(&logprobs);
                    let item = serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): item_id,
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [
                            {
                                (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                                (keys::TEXT): text,
                                (keys::ANNOTATIONS): annotations,
                                (keys::LOGPROBS): part_logprobs,
                            }
                        ]
                    });
                    // Record the finalized message item for the terminal `output[]`.
                    self.record_output_item(*index, item.clone());
                    vec![
                        event(
                            EVT_OUTPUT_TEXT_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                (keys::TEXT): text,
                                (keys::LOGPROBS): event_logprobs,
                            }),
                        ),
                        event(
                            EVT_CONTENT_PART_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (CONTENT_INDEX): 0,
                                // The finalized part, same shape as the message item's content part.
                                (PART): {
                                    (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                                    (keys::TEXT): text,
                                    (keys::ANNOTATIONS): annotations,
                                    (keys::LOGPROBS): part_logprobs,
                                }
                            }),
                        ),
                        event(
                            EVT_OUTPUT_ITEM_DONE,
                            serde_json::json!({
                                (OUTPUT_INDEX): index,
                                (ITEM_ID): item_id,
                                (ITEM): item,
                            }),
                        ),
                    ]
                } else {
                    // Nothing open here (repeated BlockStop, or a BlockStart the cap suppressed): no frame.
                    Vec::new()
                }
            }

            IrStreamEvent::MessageDelta {
                stop_reason,
                usage,
                stop_sequence: _,
                stop_detail: _,
            } => {
                // RSP-11: once `response.failed` was written (Cohere `ERROR` / Gemini
                // `MALFORMED_FUNCTION_CALL` readers emit `Error` just before `MessageDelta{Error}`),
                // the stream is over; a second terminal would tell the client the failed turn completed.
                if self.failed.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                // An unknown/None stop reason defaults to `completed`, not `failed`: a reason that
                // did not signal an error must not trigger client error handling. Genuine failures
                // arrive via `IrStreamEvent::Error`. RSP-11: an `Error` stop reason is the `failed`
                // status, carried on `response.failed` with a `server_error`.
                let generation_failed = *stop_reason == Some(crate::codec::ir::IrStopReason::Error);
                let status = if generation_failed {
                    STATUS_FAILED
                } else {
                    stop_reason
                        .map(write_responses_status)
                        .unwrap_or(STATUS_COMPLETED)
                };

                let mut resp_obj = serde_json::Map::new();
                // The terminal event's inner `response` always carries `id` and `created_at`, which
                // SDKs read to finalize the Response. Replay the id captured on `MessageStart` so it
                // matches `response.created` (a native stream never changes its id). If the cell is
                // empty (terminal before `MessageStart`, or poisoned lock), synthesize a fresh id.
                let response_id = self
                    .carried_response_id()
                    .unwrap_or_else(|| synthesize_response_id(host));
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(response_id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
                // Replay the captured `created_at` (a fresh reading would be later than the opening
                // event, a proxy tell), falling back to the caller's stamped reading.
                resp_obj.insert(
                    keys::CREATED_AT.to_string(),
                    serde_json::json!(self.carried_created_at(host)),
                );
                resp_obj.insert(keys::STATUS.to_string(), serde_json::json!(status));
                // Replay the captured `model` (required non-nullable); `carried_model` falls back to
                // DEFAULT_MODEL.
                resp_obj.insert(
                    keys::MODEL.to_string(),
                    serde_json::json!(self.carried_model()),
                );

                if status == STATUS_INCOMPLETE {
                    let reason = stop_reason
                        .map(write_responses_incomplete_reason)
                        .unwrap_or(INCOMPLETE_REASON_OTHER);
                    let mut incomplete_details = serde_json::Map::new();
                    incomplete_details.insert(keys::REASON.to_string(), serde_json::json!(reason));
                    resp_obj.insert(
                        INCOMPLETE_DETAILS.to_string(),
                        serde_json::Value::Object(incomplete_details),
                    );
                }

                // The terminal `response.usage` has the same complete shape as the non-stream body.
                resp_obj.insert(keys::USAGE.to_string(), build_responses_usage(usage));
                // The terminal `response` carries the fully assembled `output` (required), drained
                // from `output_items` in index order; empty only for an output-less turn or poisoned
                // lock. `error` is required and `null` here (failures arrive as `response.failed`).
                resp_obj.insert(
                    keys::OUTPUT.to_string(),
                    serde_json::Value::Array(self.drain_output_items()),
                );
                if generation_failed {
                    self.failed.store(true, Ordering::Relaxed);
                    resp_obj.insert(
                        keys::ERROR_WORD.to_string(),
                        serde_json::json!({ (keys::CODE): ERR_TYPE_SERVER_ERROR, (keys::MESSAGE): keys::ERROR_WORD }),
                    );
                } else {
                    resp_obj.insert(keys::ERROR_WORD.to_string(), serde_json::Value::Null);
                }
                // RSP-17: the tier that served the response, as `write_response` emits it.
                if let Some(tier) =
                    usage.detail.service_tier.as_deref().and_then(|t| {
                        crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t)
                    })
                {
                    resp_obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
                }
                // Request-echo members plus `incomplete_details: null` on a completed response (the
                // incomplete arm already set the real object, which is kept).
                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());

                // The envelope name and inner `type` must agree with `response.status`: SDKs dispatch
                // on `response.completed` vs `response.incomplete`. `status` is only `completed` or
                // `incomplete` here; the fallback is `completed`, never an invented `response.failed`.
                let event_name = match status {
                    STATUS_FAILED => EVT_RESPONSE_FAILED,
                    STATUS_INCOMPLETE => EVT_RESPONSE_INCOMPLETE,
                    _ => EVT_RESPONSE_COMPLETED,
                };
                vec![(
                    event_name.to_string(),
                    serde_json::json!({ (keys::TYPE): event_name, (keys::RESPONSE): resp_obj }),
                )]
            }

            IrStreamEvent::MessageStop => Vec::new(),

            IrStreamEvent::Error(err) => {
                // `response.failed` wraps the error in a `response` object
                // (`{"response":{"id":...,"status":"failed","error":{...}}}`); SDKs read
                // `event.response`, not a top-level `error`. A `resp_` id is synthesized for correlation.
                //
                // `response.error` is the Responses `ResponseError` shape `{"code": <non-null enum>,
                // "message": <str>}`, not the Chat `{message, type, code, param}` envelope. This
                // protocol's own reader reads `response.error.code` first and falls back to `type`.
                let message = err
                    .provider_signal
                    .clone()
                    .unwrap_or_else(|| keys::ERROR_WORD.to_string());
                // `code` must be a valid Responses enum, never the free-form `provider_signal` (a
                // sentence on cross-protocol or transport-abort paths). `message` keeps the human text.
                let code = responses_error_code(err);
                // Replay the captured `response.id` so the failure correlates with `response.created`;
                // fresh id only if the cell is empty.
                let response_id = self
                    .carried_response_id()
                    .unwrap_or_else(|| synthesize_response_id(host));
                let mut resp_obj = serde_json::Map::new();
                resp_obj.insert(keys::ID.to_string(), serde_json::json!(response_id));
                resp_obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
                // Replay the captured `created_at`; current time only if the failure preceded `MessageStart`.
                resp_obj.insert(
                    keys::CREATED_AT.to_string(),
                    serde_json::json!(self.carried_created_at(host)),
                );
                // Replay the captured `model`; DEFAULT_MODEL only if the failure preceded `MessageStart`.
                resp_obj.insert(
                    keys::MODEL.to_string(),
                    serde_json::json!(self.carried_model()),
                );
                resp_obj.insert(keys::STATUS.to_string(), serde_json::json!(STATUS_FAILED));
                // `output` is required: a failed response gets a present-but-empty array.
                resp_obj.insert(keys::OUTPUT.to_string(), serde_json::json!([]));
                resp_obj.insert(
                    keys::ERROR_WORD.to_string(),
                    serde_json::json!({ (keys::CODE): code, (keys::MESSAGE): message }),
                );
                fill_required_response_members(&mut resp_obj, self.carried_request_echo().as_ref());
                // RSP-11: terminal event; a later `MessageDelta` writes nothing.
                self.failed.store(true, Ordering::Relaxed);
                vec![event(
                    EVT_RESPONSE_FAILED,
                    serde_json::json!({ (keys::RESPONSE): resp_obj }),
                )]
            }
        };

        // Every event carries a monotonic per-stream `sequence_number`, injected here so no arm can
        // forget it. Events with no frame (`MessageStop`, empty text deltas, Image `BlockStart`) do
        // not consume a number; a multi-frame event numbers each frame in emission order.
        frames
            .into_iter()
            .map(|(event_name, mut data)| {
                if let Some(obj) = data.as_object_mut() {
                    obj.insert(
                        "sequence_number".to_string(),
                        serde_json::json!(self.next_sequence_number()),
                    );
                }
                (event_name, data)
            })
            .collect()
    }

    fn write_response(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        resp: &crate::codec::ir::IrResponse,
    ) -> serde_json::Value {
        // Unknown/None stop reasons default to `completed`; only the explicitly mapped incomplete
        // reasons downgrade the status.
        let status = resp
            .stop_reason
            .map(write_responses_status)
            .unwrap_or(STATUS_COMPLETED);

        // Build `output` in IR encounter order, one native item per block, as the streaming writer's
        // `drain_output_items` does. Collecting text separately and hoisting one coalesced message
        // to the front broke order for interleaved content and disagreed with the stream.
        let mut output_arr: Vec<serde_json::Value> = Vec::new();
        // The IR carries response logprobs once for the whole text; Responses carries them per
        // `output_text` part, so they go on the first text part and later parts get `[]`.
        let mut pending_logprobs: Option<&[crate::codec::ir::IrTokenLogprob]> =
            Some(&resp.logprobs);
        for block in &resp.content {
            match block {
                // IR-02: a refusal is a message item whose one part is the `refusal` part (as the
                // stream's refusal item finalizes).
                crate::codec::ir::IrBlock::Text {
                    text,
                    refusal: true,
                    ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_MSG),
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [{ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text }]
                    }));
                }
                crate::codec::ir::IrBlock::Text {
                    text, citations, ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    let mut annotations =
                        super::super::url_citation_wire::url_annotations(text, 0, citations);
                    annotations.extend(super::file_annotations(citations));
                    let logprobs =
                        write_responses_part_logprobs(pending_logprobs.take().unwrap_or(&[]));
                    // Match the streaming `output_item.done` message shape: item `id` (`msg_…`),
                    // `status`, and `annotations: []` on the part (missing fields are a proxy tell).
                    // Each non-empty text block is its own message item at its encounter position.
                    // `logprobs` is required on every `output_text` part.
                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_MESSAGE,
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_MSG),
                        (keys::ROLE): keys::ASSISTANT,
                        (keys::STATUS): STATUS_COMPLETED,
                        (keys::CONTENT): [{
                            (keys::TYPE): CONTENT_TYPE_OUTPUT_TEXT,
                            (keys::TEXT): text,
                            (keys::ANNOTATIONS): annotations,
                            (keys::LOGPROBS): logprobs
                        }]
                    }));
                }
                crate::codec::ir::IrBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                    output_arr.push(serde_json::json!({
                        (keys::TYPE): ITEM_TYPE_FUNCTION_CALL,
                        // Native function_call items carry an item `id` (`fc_…`) distinct from
                        // `call_id`, as the streaming `done` emits; the IR has none, so synthesize one.
                        (keys::ID): synthesize_item_id(host, ITEM_ID_PREFIX_FC),
                        (CALL_ID): id,
                        (keys::NAME): name,
                        (keys::ARGUMENTS): args_str
                    }));
                }
                // REASONING: an IR Thinking block becomes a native `reasoning` output item with the
                // text under a `content[]` `reasoning_text` part. A signature round-trips into
                // `encrypted_content` so a same-protocol hop is lossless.
                crate::codec::ir::IrBlock::Thinking {
                    text,
                    signature,
                    redacted,
                    kind,
                    ..
                } => {
                    // A redacted block holds opaque bytes with no plaintext analog: dropped, not leaked.
                    if *redacted {
                        continue;
                    }
                    let emit_sig = signature.as_deref();
                    // A wholly empty block (no text, no signature) emits no item.
                    if text.is_empty() && emit_sig.is_none() {
                        continue;
                    }
                    let mut item = serde_json::Map::new();
                    item.insert(
                        keys::TYPE.to_string(),
                        serde_json::json!(ITEM_TYPE_REASONING),
                    );
                    item.insert(
                        keys::ID.to_string(),
                        serde_json::json!(synthesize_item_id(host, ITEM_ID_PREFIX_RS)),
                    );
                    // IR-17: the same `summary[]` / `content[]` placement as the stream.
                    super::slots::insert_reasoning_text(&mut item, text, *kind);
                    if let Some(sig) = emit_sig {
                        item.insert(ENCRYPTED_CONTENT.to_string(), serde_json::json!(sig));
                    }
                    output_arr.push(serde_json::Value::Object(item));
                }
                // `output` carries only assistant `message`/`function_call` items, so ToolResult and
                // Image are dropped. Listed explicitly so a new IrBlock variant fails to compile.
                crate::codec::ir::IrBlock::ToolResult { .. } => {}
                // A model does not emit an attachment on the response surface; nothing is lost.
                crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. }
                | crate::codec::ir::IrBlock::Json(_) => {}
                // A provider-run web search is a `web_search_call` output item.
                crate::codec::ir::IrBlock::HostedToolRecord {
                    call_id,
                    status,
                    results,
                    ..
                } => output_arr.push(super::write_web_search_call(
                    host,
                    call_id.as_deref(),
                    status.as_deref(),
                    results,
                )),
            }
        }

        // `total_tokens` and the `*_details` objects are required by the SDKs; the shared builder
        // always emits them and rebuilds the cache-inclusive `input_tokens` from the IR.
        let usage_value = build_responses_usage(&resp.usage);

        let mut obj = serde_json::Map::new();
        // Top-level identity. Same-protocol carries upstream values; cross-protocol synthesizes a
        // `resp_` id and the current unix time. `created_at` is the Responses field name.
        let id = resp
            .id
            .clone()
            .unwrap_or_else(|| synthesize_response_id(host));
        let created_at = resp
            .created
            .unwrap_or_else(|| self.default_created_at(host));
        obj.insert(keys::ID.to_string(), serde_json::json!(id));
        obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_RESPONSE));
        obj.insert(keys::CREATED_AT.to_string(), serde_json::json!(created_at));
        obj.insert(keys::STATUS.to_string(), serde_json::json!(status));
        // The serving model (kept across cross-protocol translation); required, so DEFAULT_MODEL
        // when the IR carries none.
        obj.insert(
            keys::MODEL.to_string(),
            serde_json::json!(resp.model.as_deref().unwrap_or(DEFAULT_MODEL)),
        );
        obj.insert(
            keys::OUTPUT.to_string(),
            serde_json::Value::Array(output_arr),
        );
        // `output_text` is not emitted: it is an SDK-computed property, and a real body never
        // carries the key. Its data is in the `output[]` message text.
        // See `responses_response_output_and_output_text_emitted`.
        obj.insert(keys::USAGE.to_string(), usage_value);
        // RSP-17: the tier that served the response, in Responses vocabulary; omitted when absent
        // or unmappable.
        if let Some(tier) = resp
            .usage
            .detail
            .service_tier
            .as_deref()
            .and_then(|t| crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t))
        {
            obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
        }
        // `Response.error` is required on every Response: `null` on success/incomplete, as the
        // `response.created` skeleton emits. Failures arrive via `write_error`, not this body.
        obj.insert(keys::ERROR_WORD.to_string(), serde_json::Value::Null);

        if status == STATUS_INCOMPLETE {
            let reason = resp
                .stop_reason
                .map(write_responses_incomplete_reason)
                .unwrap_or(INCOMPLETE_REASON_OTHER);
            let mut incomplete_details = serde_json::Map::new();
            incomplete_details.insert(keys::REASON.to_string(), serde_json::json!(reason));
            obj.insert(
                INCOMPLETE_DETAILS.to_string(),
                serde_json::Value::Object(incomplete_details),
            );
        }
        // Request-echo members plus `incomplete_details: null` on a completed response.
        fill_required_response_members(&mut obj, resp.request_echo.as_ref());

        serde_json::Value::Object(obj)
    }

    /// Native Responses error envelope, the OpenAI shape SDKs decode into `APIError`:
    /// `{"error":{"message","type","code":null,"param":null}}` as `application/json`. The generic
    /// `kind` maps to the Responses `type` vocabulary where one exists.
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        _status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        // Map the generic `kind` to the OpenAI `error.type` vocabulary (`invalid_request_error`,
        // `authentication_error`, `permission_error`, `not_found_error`, `rate_limit_error`,
        // `server_error`, `insufficient_quota`). Anything already in it, or unrecognized, passes
        // through verbatim so a precise upstream type is not lost.
        let error_type = match kind {
            "invalid_request" | ERR_TYPE_INVALID_REQUEST => ERR_TYPE_INVALID_REQUEST,
            "authentication" | ERR_TYPE_AUTHENTICATION | "auth" => ERR_TYPE_AUTHENTICATION,
            "permission" | ERR_TYPE_PERMISSION | "forbidden" => ERR_TYPE_PERMISSION,
            "not_found" | ERR_TYPE_NOT_FOUND => ERR_TYPE_NOT_FOUND,
            "rate_limit" | ERR_TYPE_RATE_LIMIT => ERR_TYPE_RATE_LIMIT,
            ERR_TYPE_SERVER_ERROR | "internal" | "internal_error" => ERR_TYPE_SERVER_ERROR,
            // `overloaded` is Anthropic vocabulary; OpenAI reports a 5xx as `server_error`. Without
            // this arm it would leak as a non-native type. Same bucket as the OpenAI writer's 5xx.
            busbar_contract::protocol::KIND_OVERLOADED
            | ERR_TYPE_OVERLOADED
            | "service_unavailable"
            | "unavailable" => ERR_TYPE_SERVER_ERROR,
            // Transient/upstream-failure kinds the engine emits to every ingress writer
            // (`timeout`/`network`/`connect`, `5xx`/`transient`, `api_error`) are not OpenAI types;
            // real OpenAI reports them as `server_error`. Mirrors openai_chat.rs's bucket.
            crate::codec::framework::KIND_TIMEOUT
            | "network"
            | "connect"
            | "5xx"
            | "transient"
            | busbar_contract::protocol::KIND_API_ERROR => ERR_TYPE_SERVER_ERROR,
            // No dedicated Responses type for context-length overflow (openai_chat.rs also folds it)
            // so it becomes `invalid_request_error`, as does `bad_request`.
            crate::codec::framework::PROVIDER_CODE_CONTEXT_LENGTH | "bad_request" => {
                ERR_TYPE_INVALID_REQUEST
            }
            "billing" | ERR_TYPE_INSUFFICIENT_QUOTA => ERR_TYPE_INSUFFICIENT_QUOTA,
            other => other,
        };

        serde_json::json!({
            (keys::ERROR_WORD): {
                (keys::MESSAGE): message,
                (keys::TYPE): error_type,
                (keys::CODE): bearer_error_code(error_type),
                "param": serde_json::Value::Null,
            }
        })
    }

    /// IR-18: `encrypted_content` is read back as an OpenAI-minted blob.
    fn reads_signature_origin_as_own(&self, origin: crate::codec::ir::IrSignatureOrigin) -> bool {
        origin == crate::codec::ir::IrSignatureOrigin::OpenAi
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }
}

/// One IR image to a Responses `input_image` part, or `None` when the source has no Responses
/// form. Shared by message content and `function_call_output` content (RSP-10).
///
/// IR-08: `detail` is written beside the source when the IR carries one.
fn input_image_part(
    source: &crate::codec::ir::IrImageSource,
    detail: Option<crate::codec::ir::IrImageDetail>,
) -> Option<serde_json::Value> {
    let mut part = input_image_source_part(source)?;
    if let (Some(detail), Some(obj)) = (detail, part.as_object_mut()) {
        obj.insert(keys::DETAIL.to_string(), serde_json::json!(detail.as_str()));
    }
    Some(part)
}

/// The source half of [`input_image_part`].
fn input_image_source_part(source: &crate::codec::ir::IrImageSource) -> Option<serde_json::Value> {
    // SHR-03: an OpenAI Files id (this dialect's `input_image.file_id` or a Chat `file_id`, one
    // namespace) re-emits as `file_id`; a data URI would corrupt it.
    if let Some(id) = super::super::url_citation_wire::files_api_id(source) {
        return Some(serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::FILE_ID): id }));
    }
    match source {
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            value
                .get(keys::FILE_ID)
                .and_then(|i| i.as_str())
                .map(|id| serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::FILE_ID): id }))
        }
        // A foreign vendor reference (a Bedrock s3Location) has no analog: drop with a warn.
        crate::codec::ir::IrImageSource::Vendor { .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::IMAGE,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [],
                "dropping unresolvable foreign vendor image reference on Responses egress: no \
                 cross-vendor analog");
            None
        }
        // A URL/base64 image reconstructs the original `image_url`.
        url_or_b64 => super::super::ir_encode::image_url_from_ir(url_or_b64)
            .map(|image_url| serde_json::json!({ (keys::TYPE): INPUT_IMAGE, (keys::IMAGE_URL): image_url })),
    }
}

/// One IR attachment to a Responses `input_file` part, or `None` when it has no Responses form.
/// Only a document projects (no audio or video part); other kinds are dropped with a warn naming
/// the construct. Shared by message and `function_call_output` content (RSP-10).
fn input_file_part(
    kind: crate::codec::ir::IrMediaKind,
    source: &crate::codec::ir::IrImageSource,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    if kind != crate::codec::ir::IrMediaKind::Document {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::block(kind.as_str()),
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [media_kind = kind.as_str(),],
            "dropping attachment on Responses egress: the input surface has an `input_file` part \
             and no audio or video part; the block is NOT emitted"
        );
        return None;
    }
    let mut part = serde_json::Map::new();
    part.insert(keys::TYPE.to_string(), serde_json::json!(INPUT_FILE));
    match source {
        crate::codec::ir::IrImageSource::Base64 { media_type, data } => {
            part.insert(
                keys::FILE_DATA.to_string(),
                serde_json::json!(format!("data:{media_type};base64,{data}")),
            );
        }
        crate::codec::ir::IrImageSource::Url(url) => {
            part.insert(FILE_URL.to_string(), serde_json::json!(url));
        }
        // SHR-03: an OpenAI Files id (this dialect's `input_file.file_id` or a Chat
        // `file.file_id`) re-emits as `input_file.file_id`.
        crate::codec::ir::IrImageSource::Vendor { .. }
            if super::super::url_citation_wire::files_api_id(source).is_some() =>
        {
            let id = super::super::url_citation_wire::files_api_id(source)?;
            part.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
        }
        // This protocol's own uploads handle round-trips verbatim; a foreign one (Bedrock
        // s3Location, Anthropic Files id) is unresolvable here.
        crate::codec::ir::IrImageSource::Vendor { vendor, value } if *vendor == VENDOR_NAME => {
            let id = value.get(keys::FILE_ID).and_then(|i| i.as_str())?;
            part.insert(keys::FILE_ID.to_string(), serde_json::json!(id));
        }
        crate::codec::ir::IrImageSource::Vendor { vendor, .. } => {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::DOCUMENT,
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                [vendor = %vendor, ],
                "dropping document attachment on Responses egress: the source is a foreign vendor \
                 file handle this backend cannot resolve; the block is NOT emitted");
            return None;
        }
    }
    if let Some(n) = name {
        part.insert(keys::FILENAME.to_string(), serde_json::json!(n));
    }
    Some(serde_json::Value::Object(part))
}

/// A `ToolResult`'s content as the `function_call_output.output` value (RSP-10): a string, or an
/// array of `input_text` / `input_image` / `input_file` parts. An all-text result stays a string,
/// adjacent text joined without a separator (a space corrupts base64 or split JSON, as in the Chat
/// writer). A structured-JSON block is serialized into that text. The part array is used only when
/// an image or attachment is present.
fn function_call_output_value(content: &[crate::codec::ir::IrBlock]) -> serde_json::Value {
    let text_of = |b: &crate::codec::ir::IrBlock| -> Option<String> {
        match b {
            crate::codec::ir::IrBlock::Text { text, .. } => Some(text.clone()),
            crate::codec::ir::IrBlock::Json(v) => Some(v.to_string()),
            _ => None,
        }
    };
    let has_attachment = content.iter().any(|b| {
        matches!(
            b,
            crate::codec::ir::IrBlock::Image { .. } | crate::codec::ir::IrBlock::Media { .. }
        )
    });
    if !has_attachment {
        return serde_json::json!(content.iter().filter_map(text_of).collect::<String>());
    }
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for block in content {
        match block {
            crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                parts.extend(input_image_part(source, *detail))
            }
            crate::codec::ir::IrBlock::Media {
                kind, source, name, ..
            } => parts.extend(input_file_part(*kind, source, name.as_deref())),
            other => {
                if let Some(text) = text_of(other) {
                    parts
                        .push(serde_json::json!({ (keys::TYPE): CONTENT_TYPE_INPUT_TEXT, (keys::TEXT): text }));
                }
            }
        }
    }
    if parts.is_empty() {
        // Every attachment was unrepresentable (warned above) and no text: the empty string.
        return serde_json::json!("");
    }
    serde_json::Value::Array(parts)
}
