use super::*;

/// The `created` stamped when the answer carries none: the host's wall clock via the
/// [`CodecHost`](busbar_contract::codec::CodecHost) (the plane reads no clock of its own), or `0`
/// (what an unstamped Responses answer carries) when the host holds none.
fn host_now(host: &dyn busbar_contract::codec::CodecHost) -> u64 {
    host.wall_clock_now().unwrap_or(0)
}

/// One streamed `chat.completion.chunk` frame carrying `delta` on its single choice, unfinished.
fn delta_chunk(delta: serde_json::Value) -> Option<(String, serde_json::Value)> {
    Some((
        String::new(),
        serde_json::json!({
            (keys::OBJECT): OBJ_CHUNK,
            (CHOICES): [{
                (keys::INDEX): 0,
                (keys::DELTA): delta,
                (keys::FINISH_REASON): null
            }]
        }),
    ))
}

impl ProtocolWriter for OpenAiWriter {
    fn upstream_path(&self) -> &str {
        PATH_UPSTREAM
    }

    /// The candidate count is top-level `n`. `Some(k)` only for `k > 1`; absent, 1 or unparseable
    /// is `None`. The engine rejects `n > 1` cross-protocol (the single-candidate IR would drop
    /// candidates `1..n`).
    fn requested_candidate_count(&self, body: &serde_json::Value) -> Option<u64> {
        body.get(W_N).and_then(|v| v.as_u64()).filter(|&k| k > 1)
    }

    fn write_request(
        &self,
        host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
    ) -> serde_json::Value {
        self.write_request_for_lane(host, req, "", &LaneCaps::default())
    }

    /// A hosted tool Chat has no built-in for (code execution, web fetch) is dropped by the writer;
    /// the seam records each as a dropped control.
    fn dropped_egress_controls(&self, req: &crate::codec::ir::IrRequest) -> Vec<&'static str> {
        crate::codec::carry::dropped(super::map::REQUEST, super::map::CONTROLS, req)
            .map(crate::codec::carry::Slot::name)
            .chain(super::slots::dropped_hosted_kinds(req))
            .collect()
    }

    fn write_request_for_lane(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        req: &crate::codec::ir::IrRequest,
        _model: &str,
        caps: &LaneCaps,
    ) -> serde_json::Value {
        // The request controls with no Chat form, as `dropped_egress_controls` reports.
        crate::codec::carry::warn_drops(super::map::REQUEST, super::map::CONTROLS, None, req);
        let mut messages_array: Vec<serde_json::Value> = Vec::new();

        // System message first, if present. It carries plain text only, so every block is projected
        // explicitly: Text and Thinking are forwarded; the text-less variants (ToolUse / ToolResult
        // / Image) become empty text, a documented lossy projection. The match is exhaustive so a
        // new IrBlock variant fails to compile. The folded turn keeps the role the caller wrote
        // (`developer` from a Chat / Responses developer message), else `system`.
        let system_role = req
            .system_role
            .map_or(keys::SYSTEM, crate::codec::ir::IrSystemRole::as_str);
        for block in &req.system {
            let text: &str = match block {
                crate::codec::ir::IrBlock::Text { text, .. } => text,
                crate::codec::ir::IrBlock::Thinking { text, .. } => text,
                crate::codec::ir::IrBlock::ToolUse { .. }
                | crate::codec::ir::IrBlock::ToolResult { .. }
                | crate::codec::ir::IrBlock::Image { .. }
                | crate::codec::ir::IrBlock::Media { .. }
                | crate::codec::ir::IrBlock::Json(_)
                | crate::codec::ir::IrBlock::HostedToolRecord { .. } => "",
            };
            messages_array.push(serde_json::json!({
                (keys::ROLE): system_role,
                (keys::CONTENT): text
            }));
        }

        // Participant names parked by this protocol's own reader (`MESSAGE_NAMES_SENTINEL`), present
        // only same-protocol (the seam clears `extra`), the only scope `name` survives in.
        let message_names = req
            .extra
            .get(MESSAGE_NAMES_SENTINEL)
            .and_then(|v| v.as_object());
        // Provider-specific fields parked by this protocol's own reader (`MESSAGE_EXTRAS_SENTINEL`),
        // same-protocol only like `message_names`; an assistant `audio` reference or legacy
        // `function_call` has no home elsewhere.
        let message_extras = req
            .extra
            .get(MESSAGE_EXTRAS_SENTINEL)
            .and_then(|v| v.as_object());

        // Add regular messages
        for (msg_idx, msg) in req.messages.iter().enumerate() {
            let role_str = match msg.role {
                crate::codec::ir::IrRole::User => keys::USER,
                crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                crate::codec::ir::IrRole::Tool => keys::TOOL,
                crate::codec::ir::IrRole::System => keys::SYSTEM,
            };

            let content_val: serde_json::Value = if msg.content.is_empty() {
                serde_json::json!("")
            } else {
                let mut content_arr: Vec<serde_json::Value> = Vec::new();

                for block in &msg.content {
                    match block {
                        // A refusal in replayed assistant history is Chat's `refusal` part
                        // (assistant content only; any other role keeps it as text).
                        crate::codec::ir::IrBlock::Text {
                            text,
                            refusal: true,
                            ..
                        } if msg.role == crate::codec::ir::IrRole::Assistant => {
                            content_arr
                                .push(serde_json::json!({ (keys::TYPE): keys::REFUSAL, (keys::REFUSAL): text }));
                        }
                        crate::codec::ir::IrBlock::Text { text, .. } => {
                            content_arr.push(
                                serde_json::json!({ (keys::TYPE): keys::TEXT, (keys::TEXT): text }),
                            );
                        }
                        crate::codec::ir::IrBlock::Image { source, detail, .. } => {
                            // A URL is verbatim, base64 is re-wrapped as a data URI. A Responses
                            // `file_id` / Bedrock `s3Location` has no `image_url` projection: skip
                            // it with a warn rather than corrupt the block.
                            match super::super::ir_encode::image_url_from_ir(source) {
                                Some(url) => {
                                    let mut image_url = serde_json::json!({ (keys::URL): url });
                                    // The image-fidelity ask, when the source stated one.
                                    if let Some(d) = detail {
                                        image_url[keys::DETAIL] = serde_json::json!(d.as_str());
                                    }
                                    content_arr.push(serde_json::json!({
                                        (keys::TYPE): keys::IMAGE_URL,
                                        (keys::IMAGE_URL): image_url
                                    }))
                                }
                                None => crate::codec::drops::writer_drop!(
                                    crate::codec::drops::IMAGE,
                                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                                    [],
                                    "dropping unresolvable vendor-scoped image reference on OpenAI \
                                     egress: a Responses input_image.file_id or a Bedrock s3Location \
                                     has no cross-vendor analog; the block is NOT emitted"),
                            }
                        }
                        crate::codec::ir::IrBlock::Media {
                            kind, source, name, ..
                        } => {
                            if let Some(part) =
                                super::media_part_from_ir(*kind, source, name.as_deref())
                            {
                                content_arr.push(part);
                            }
                        }
                        crate::codec::ir::IrBlock::ToolUse { .. } => {
                            // ToolUse is surfaced via the `tool_calls` array below, not content.
                        }
                        crate::codec::ir::IrBlock::ToolResult { .. } => {
                            // ToolResult is rendered as a standalone `{"role":"tool",...}` entry by the
                            // tool-result path below; it has no content representation here.
                        }
                        crate::codec::ir::IrBlock::Thinking { .. } => {
                            // Chat has no thinking block on request input; dropped (lossy by necessity).
                        }
                        crate::codec::ir::IrBlock::Json(_)
                        | crate::codec::ir::IrBlock::HostedToolRecord { .. } => {
                            // Structured-json (a Bedrock tool-result member) has no content shape; dropped.
                        }
                    }
                }

                // A tool-call-only assistant turn yields an empty content_arr; Chat expects
                // `content: null`, not `[]` (some validators reject an empty array beside `tool_calls`).
                if content_arr.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Array(content_arr)
                }
            };

            let mut msg_obj = serde_json::json!({
                (keys::ROLE): role_str,
                (keys::CONTENT): content_val,
            });

            // Re-emit the participant `name`: a pool-alias route re-serializes rather than forwarding
            // the caller's bytes and would otherwise strip it on a same-protocol hop.
            if let Some(name) = message_names
                .and_then(|m| m.get(&msg_idx.to_string()))
                .and_then(|v| v.as_str())
            {
                if let Some(o) = msg_obj.as_object_mut() {
                    o.insert(keys::NAME.to_string(), serde_json::json!(name));
                }
            }

            // Re-attach parked provider-specific fields (`MESSAGE_EXTRAS_SENTINEL`). `or_insert`
            // keeps an already-emitted modeled key from being overwritten by a stale stash.
            if let Some(ex) = message_extras
                .and_then(|m| m.get(&msg_idx.to_string()))
                .and_then(|v| v.as_object())
            {
                if let Some(o) = msg_obj.as_object_mut() {
                    for (k, v) in ex {
                        if k == LEGACY_FUNCTION_ROLE_KEY {
                            continue;
                        }
                        o.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }

            // Emit tool_calls for any message carrying ToolUse, not only assistant ones: a ToolUse
            // on another role is legal in the IR, and gating on assistant silently dropped it.
            {
                let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        // Serialize input to JSON string
                        let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                        // The tool_call id is verbatim so it correlates with the result's `tool_call_id`.
                        tool_calls_arr.push(serde_json::json!({
                            (keys::TYPE): TOOL_TYPE_FUNCTION,
                            (keys::ID): id,
                            (keys::FUNCTION): {
                                (keys::NAME): name,
                                (keys::ARGUMENTS): args_str
                            }
                        }));
                    }
                }

                // A legacy `function_call` re-attached from the extras stash already carries the
                // call; `tool_calls` would duplicate it.
                if !tool_calls_arr.is_empty() && msg_obj.get(keys::FUNCTION_CALL).is_none() {
                    msg_obj[keys::TOOL_CALLS] = serde_json::Value::Array(tool_calls_arr);
                }
            }

            // Tool results: a flat `{"role":"tool",...}` entry for any message carrying ToolResult
            // blocks, whatever its role. A Gemini `functionResponse` and an Anthropic `tool_result`
            // both decode to a User-role message, so gating on IrRole::Tool silently dropped them.
            let has_tool_result = msg
                .content
                .iter()
                .any(|b| matches!(b, crate::codec::ir::IrBlock::ToolResult { .. }));
            if has_tool_result {
                let mut emitted_tool_result = false;
                for block in &msg.content {
                    if let crate::codec::ir::IrBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        let mut tool_result_obj = serde_json::json!({
                            (keys::ROLE): keys::TOOL,
                            (keys::TOOL_CALL_ID): tool_use_id,
                            (keys::CONTENT): "",
                        });

                        // Concatenate text with no separator, as the read path does: a space would
                        // corrupt boundary-sensitive content (base64, JSON split across blocks).
                        if !content.is_empty() {
                            let text_parts: Vec<String> = content
                                .iter()
                                .filter_map(|b| match b {
                                    crate::codec::ir::IrBlock::Text { text, .. } => {
                                        Some(text.clone())
                                    }
                                    // A structured-json block (Bedrock `{"json":…}`) is written as
                                    // its serialized JSON string; dropping it left the model an
                                    // empty tool result.
                                    crate::codec::ir::IrBlock::Json(v) => Some(v.to_string()),
                                    // An image / media / other block has no slot in string content.
                                    _ => None,
                                })
                                .collect();

                            tool_result_obj[keys::CONTENT] = serde_json::json!(text_parts.concat());
                        }

                        // A legacy `role:"function"` result is written back in its legacy shape.
                        if let Some(fname) = message_extras
                            .and_then(|m| m.get(&msg_idx.to_string()))
                            .and_then(|e| e.get(LEGACY_FUNCTION_ROLE_KEY))
                        {
                            tool_result_obj = serde_json::json!({
                                (keys::ROLE): keys::FUNCTION,
                                (keys::NAME): fname,
                                (keys::CONTENT): tool_result_obj[keys::CONTENT].clone(),
                            });
                        }
                        messages_array.push(tool_result_obj);
                        emitted_tool_result = true;
                    }
                }

                // A message with only ToolResult blocks emitted them above as standalone entries and
                // adds no `msg_obj`. It can also carry other content (e.g. a Gemini turn pairing a
                // functionResponse with narration text), which was once silently dropped: push
                // `msg_obj` when it has a non-null `content` or a `tool_calls` array, or when the
                // message had no ToolResult at all. This never duplicates a ToolResult.
                let msg_has_payload = msg_obj.get(keys::CONTENT).is_some_and(|c| !c.is_null())
                    || msg_obj.get(keys::TOOL_CALLS).is_some();
                if msg_has_payload || !emitted_tool_result {
                    messages_array.push(msg_obj);
                }
            } else {
                // No ToolResult content: add the message directly.
                //
                // `content: null` is legal only beside `tool_calls` (or a legacy `function_call`). A
                // turn with no Chat projection (only Thinking, or an inexpressible attachment) would
                // go out as `{"role":"assistant","content":null}`, a 400. Send the empty string: the
                // turn keeps its place and says nothing.
                if let Some(o) = msg_obj.as_object_mut() {
                    let content_null = o.get(keys::CONTENT).is_some_and(|c| c.is_null());
                    if content_null
                        && !o.contains_key(keys::TOOL_CALLS)
                        && !o.contains_key(keys::FUNCTION_CALL)
                    {
                        o.insert(keys::CONTENT.to_string(), serde_json::json!(""));
                    }
                }
                messages_array.push(msg_obj);
            }
        }

        let mut out = serde_json::Map::new();

        // Add model from extra if present (since IrRequest doesn't have a model field)
        if let Some(model_val) = req.extra.get(keys::MODEL) {
            out.insert(keys::MODEL.to_string(), model_val.clone());
        }

        out.insert(
            keys::MESSAGES.to_string(),
            serde_json::Value::Array(messages_array),
        );

        // The modeled output-token cap. The reader folds `max_tokens` and `max_completion_tokens`
        // into one IR field and marks the latter with `MAX_COMPLETION_TOKENS_SENTINEL`, so an
        // OpenAI-origin IR re-emits that key.
        //
        // A cap with no sentinel (another dialect's, or the seam's injected cap) has a spelling
        // that is a lane fact: o-series / gpt-5 models reject `max_tokens`, while a compatible
        // host unaware of `max_completion_tokens` would run uncapped. The lane's provider entry
        // declares it (`LaneCaps::max_output_key`); the default is `max_tokens`.
        if let Some(max_tokens) = req.max_tokens {
            let completion_key = req
                .extra
                .get(MAX_COMPLETION_TOKENS_SENTINEL)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || caps.max_output_key == MaxOutputKey::MaxCompletionTokens;
            let key = if completion_key {
                MAX_COMPLETION_TOKENS
            } else {
                keys::MAX_TOKENS
            };
            out.insert(key.to_string(), serde_json::json!(max_tokens));
        }

        // The sampling controls (temperature, top_p, stop capped at 4, the penalties, seed, n)
        // are rows of the mapping file, written with the other rows below. OpenAI has NO top_k
        // parameter: a source protocol's top_k is not written (lossy-by-target).

        // The Anthropic-analog carries, re-emitted in OpenAI's native spelling (an Anthropic
        // `metadata.user_id` arrives here as `user`; `disable_parallel_tool_use` arrives inverted).
        if let Some(user) = &req.user {
            out.insert(keys::USER.to_string(), serde_json::json!(user));
        }
        // OpenAI rejects `parallel_tool_calls` without tools, but an Anthropic source can carry the
        // flag on a tool-less request, so gate on tools (as the Anthropic writer does).
        if let (Some(parallel), false) = (req.parallel_tool_calls, req.tools.is_empty()) {
            out.insert(
                keys::PARALLEL_TOOL_CALLS.to_string(),
                serde_json::json!(parallel),
            );
        }
        // The reasoning carry as `reasoning_effort`; a numeric budget (Anthropic/Gemini) is bucketized
        // through the effort table.
        //
        // `Off` is matched first: through the table it would read as the smallest enable ask.
        // `"none"` is accepted only by the newest reasoning models, so a lane that declares it
        // (`LaneCaps::reasoning_none`) gets `"none"` and any other omits the ask. An OpenAI-origin
        // `"none"` still rides `extra` verbatim.
        if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) && caps.reasoning_none {
            out.insert(
                REASONING_EFFORT.to_string(),
                serde_json::json!(keys::NONE_WORD),
            );
        } else if req.reasoning == Some(crate::codec::ir::IrReasoningAsk::Off) {
            crate::codec::drops::writer_drop!(
                crate::codec::drops::REASONING,
                &crate::codec::diagnostics::IR_DROP_REASONING,
                [],
                "omitting reasoning OFF on OpenAI Chat egress: reasoning_effort \"none\" is not \
                 accepted by every OpenAI reasoning model and this lane does not declare it"
            );
        } else if let Some(ask) = req.reasoning {
            let table = req
                .reasoning_budgets
                .unwrap_or(crate::codec::ir::REASONING_BUDGET_DEFAULTS);
            match ask.to_effort(table) {
                Some(effort) => {
                    out.insert(
                        REASONING_EFFORT.to_string(),
                        serde_json::json!(effort.as_three_word_str()),
                    );
                }
                // "The model decides" has no effort word: dropped, and the model runs at its default.
                None => crate::codec::drops::writer_drop!(
                    crate::codec::drops::REASONING,
                    &crate::codec::diagnostics::IR_DROP_REASONING,
                    [],
                    "dropping a \"model decides\" reasoning ask on OpenAI Chat egress: \
                     reasoning_effort has no dynamic form"
                ),
            }
        }
        // The logprobs ask. OpenAI requires `logprobs: true` for `top_logprobs`, so force the flag
        // whenever the count is present (a Gemini request may carry only `logprobs: N`), or it 400s.
        if let Some(top_logprobs) = req.top_logprobs {
            out.insert(keys::LOGPROBS.to_string(), serde_json::json!(true));
            out.insert(
                keys::TOP_LOGPROBS.to_string(),
                serde_json::json!(top_logprobs),
            );
        } else if let Some(logprobs) = req.logprobs {
            out.insert(keys::LOGPROBS.to_string(), serde_json::json!(logprobs));
        }
        if let Some(response_format) = &req.response_format {
            out.insert(
                keys::RESPONSE_FORMAT.to_string(),
                write_openai_response_format(response_format),
            );
        }

        out.insert(keys::STREAM.to_string(), serde_json::json!(req.stream));

        // Tools. Chat requires the nested shape `{"type":"function","function":{"name",
        // "description","parameters"}}`; the flat shape is a 400 on every native backend and a proxy
        // tell. `read_openai_tool` reads the nested object, so this is its inverse.
        // An OpenAI-origin request that declared legacy `functions` carries that array verbatim in
        // `extra`; emitting `tools` too would declare every function twice.
        // The typed custom tools are written after the function tools.
        let custom_tools = super::slots::write_custom_tools(req);
        if (!req.tools.is_empty() || !custom_tools.is_empty()) && !req.extra.contains_key(FUNCTIONS)
        {
            let mut tools_arr: Vec<serde_json::Value> = Vec::new();
            for tool in &req.tools {
                // A non-function tool this reader carried as `hosted` (a Chat `custom` tool) is
                // re-emitted verbatim. Any other hosted definition (a Responses built-in) has no
                // Chat shape; the seam removes those, and one that got here is not emitted as a
                // malformed empty-name function.
                if let Some(hosted) = &tool.hosted {
                    if hosted.get(keys::TYPE).and_then(|t| t.as_str()) == Some(TOOL_TYPE_CUSTOM) {
                        tools_arr.push(hosted.clone());
                    } else {
                        crate::codec::drops::writer_drop!(
                            crate::codec::drops::TOOLS,
                            &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
                            [],
                            "dropping a hosted tool on OpenAI Chat egress: it has no Chat \
                             Completions tool shape"
                        );
                    }
                    continue;
                }
                let mut function_obj = serde_json::Map::new();
                function_obj.insert(keys::NAME.to_string(), serde_json::json!(tool.name));

                if let Some(desc) = &tool.description {
                    function_obj.insert(keys::DESCRIPTION.to_string(), serde_json::json!(desc));
                }

                // Map OpenAI's "parameters" to our input_schema
                let params = if !tool.input_schema.is_null() {
                    tool.input_schema.clone()
                } else {
                    serde_json::json!({})
                };
                function_obj.insert(keys::PARAMETERS.to_string(), params);

                // Strict function calling, in its nested slot, only when the source stated it:
                // `None` is "said nothing", and `strict: false` would invent an opt-out.
                if let Some(strict) = tool.strict {
                    function_obj.insert(keys::STRICT.to_string(), serde_json::json!(strict));
                }

                let mut tool_obj = serde_json::Map::new();
                tool_obj.insert(
                    keys::TYPE.to_string(),
                    serde_json::json!(TOOL_TYPE_FUNCTION),
                );
                tool_obj.insert(
                    keys::FUNCTION.to_string(),
                    serde_json::Value::Object(function_obj),
                );

                tools_arr.push(serde_json::Value::Object(tool_obj));
            }
            tools_arr.extend(custom_tools.iter().cloned());
            if !tools_arr.is_empty() {
                out.insert(keys::TOOLS.to_string(), serde_json::Value::Array(tools_arr));
            }
        }

        // `tool_choice` in OpenAI's shape. OpenAI 400s `tool_choice` without `tools`, so one left
        // with no tools (e.g. `prepare_for_egress` stripped its hosted tools) degrades with a warn.
        // The legacy `function_call` directive rides `extra` verbatim on an OpenAI-origin IR. An
        // allowed-tools subset is written as Chat's `{type:"allowed_tools", ...}` choice.
        if let Some(v) = super::slots::write_tool_choice(req)
            .filter(|_| !req.extra.contains_key(keys::FUNCTION_CALL))
        {
            if req.tools.is_empty() && custom_tools.is_empty() {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOL_CHOICE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [],
                    "dropping tool_choice on OpenAI egress: \"tool_choice\" is only allowed when \
                     \"tools\" are specified (likely because the hosted tools that carried it were \
                     stripped on the cross-protocol seam)"
                );
            } else {
                out.insert(keys::TOOL_CHOICE.to_string(), v);
            }
        }

        // The flat request fields (`map.gen.rs`). Written before `extra`, which wins on the same
        // dialect (the reader parks there only a raw member the slot cannot reproduce).
        crate::codec::carry::write_fields(
            super::map::REQUEST,
            req,
            crate::codec::carry::Egress::default(),
            &mut out,
        );

        // Add extra fields
        for (key, value) in &req.extra {
            // Both busbar-internal markers were consumed above (one selected the cap's key, the
            // other re-attached `messages[].name`); neither may reach the wire (an invalid body and a
            // proxy tell).
            if key == MAX_COMPLETION_TOKENS_SENTINEL
                || key == MESSAGE_NAMES_SENTINEL
                || key == MESSAGE_EXTRAS_SENTINEL
            {
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
                id,
                created,
                model,
                ..
            } => {
                let openai_role = match role {
                    crate::codec::ir::IrRole::Assistant => keys::ASSISTANT,
                    crate::codec::ir::IrRole::User
                    | crate::codec::ir::IrRole::System
                    | crate::codec::ir::IrRole::Tool => return None,
                };
                let delta_obj = serde_json::json!({ (keys::ROLE): openai_role });
                // The opening chunk carries the stream identity (`id`, `created`, `model`); the SDKs
                // latch it from the first chunk that supplies it. With none from the backend
                // (cross-protocol), synthesize a correct id/created. The first opening chunk decides
                // the id; a duplicate replays it.
                let chunk_id = self
                    .carried_chunk_id(|| id.clone().unwrap_or_else(|| synth_completion_id(host)));
                let chunk_created = created.unwrap_or_else(|| host_now(host));
                // `model` is required and non-nullable in the chunk schema; a backend IR with
                // `model: None` (e.g. Bedrock) falls back to DEFAULT_MODEL, since a model-less chunk
                // fails strict SDK deserialisation and is a proxy tell.
                let chunk_model = model_or_default(model.as_deref());
                let chunk = serde_json::json!({
                    (keys::ID): chunk_id,
                    (keys::OBJECT): OBJ_CHUNK,
                    (CREATED): chunk_created,
                    (keys::MODEL): chunk_model,
                    (CHOICES): [{
                        (keys::INDEX): 0,
                        (keys::DELTA): delta_obj,
                        (keys::FINISH_REASON): null
                    }]
                });
                Some(("".to_string(), chunk))
            }
            IrStreamEvent::BlockStart {
                index,
                block,
                refusal,
            } => match block {
                // A refusal text block: its deltas are written as `delta.refusal`.
                crate::codec::ir::IrBlockMeta::Text => {
                    if *refusal {
                        self.mark_refusal_block(*index);
                    }
                    None
                }
                crate::codec::ir::IrBlockMeta::ToolUse { id, name } => {
                    // Stamp the canonical IR block index so parallel calls keep distinct keys and a
                    // call's BlockStart and deltas share one value. It need not be 0-based (text at
                    // block 0, then tool_use at 1); OpenAI requires `tool_calls[].index` to enumerate
                    // from 0, so the framing seam (`OpenAiStreamFraming::remap_tool_call_index`)
                    // remaps each distinct raw index to its ordinal on egress. The writer stays 1:1
                    // and stateless; only the framing sees the whole stream.
                    let delta_obj = serde_json::json!({
                        (keys::TOOL_CALLS): [{
                            (keys::INDEX): index,
                            (keys::ID): id,
                            (keys::TYPE): TOOL_TYPE_FUNCTION,
                            (keys::FUNCTION): { (keys::NAME): name, (keys::ARGUMENTS): "" }
                        }]
                    });
                    delta_chunk(delta_obj)
                }
                // Chat has no streamed thinking/redacted-thinking/image start; a redacted block is
                // dropped like plaintext thinking.
                crate::codec::ir::IrBlockMeta::Thinking { .. }
                | crate::codec::ir::IrBlockMeta::RedactedThinking
                | crate::codec::ir::IrBlockMeta::Image => None,
            },
            IrStreamEvent::BlockDelta { index, delta } => match delta {
                crate::codec::ir::IrDelta::TextDelta(text) => {
                    let delta_obj = if self.is_refusal_block(*index) {
                        serde_json::json!({ (keys::REFUSAL): text })
                    } else {
                        serde_json::json!({ (keys::CONTENT): text })
                    };
                    delta_chunk(delta_obj)
                }
                crate::codec::ir::IrDelta::InputJsonDelta(json) => {
                    // Mirror the raw index of the matching BlockStart; the framing seam remaps it to
                    // the same ordinal so fragments reach the right parallel call.
                    let delta_obj = serde_json::json!({
                        (keys::TOOL_CALLS): [{
                            (keys::INDEX): index,
                            (keys::FUNCTION): { (keys::ARGUMENTS): json }
                        }]
                    });
                    delta_chunk(delta_obj)
                }
                crate::codec::ir::IrDelta::ThinkingDelta(_) => {
                    // Lossy by necessity: no thinking stream equivalent.
                    None
                }
                // A Chat chunk has no generated-image member (streamed audio is not carried).
                crate::codec::ir::IrDelta::SignatureDelta(_)
                | crate::codec::ir::IrDelta::RedactedReasoningDelta(_)
                | crate::codec::ir::IrDelta::MediaDelta(_) => {
                    // Lossy by necessity: no signature/redacted-reasoning stream analog.
                    None
                }
                crate::codec::ir::IrDelta::CitationsDelta(cits) => {
                    // A chunk carries `choices[].delta.annotations` in the same `url_citation` shape
                    // as the buffered `message.annotations` (see `write_response`), so sources
                    // appear at `stream:true` as at `stream:false`.
                    //
                    // Offsets: the streamed text is not assembled here, so pass empty text and a zero
                    // base; an unresolvable span is emitted without one rather than a fabricated one.
                    let annotations =
                        super::super::url_citation_wire::chat_url_annotations("", 0, cits);
                    if annotations.is_empty() {
                        return None;
                    }
                    delta_chunk(serde_json::json!({ (keys::ANNOTATIONS): annotations }))
                }
                crate::codec::ir::IrDelta::LogprobsDelta(lps) => {
                    // Streamed logprobs (e.g. Gemini's per-chunk `logprobsResult`) as
                    // `choices[].logprobs.content[]` beside an empty delta; the SDK accumulates them
                    // independently of content. An empty vec emits no chunk.
                    if lps.is_empty() {
                        None
                    } else {
                        let chunk_obj = serde_json::json!({
                            (keys::OBJECT): OBJ_CHUNK,
                            (CHOICES): [{
                                (keys::INDEX): 0,
                                (keys::DELTA): {},
                                (keys::LOGPROBS): crate::codec::logprob_wire::write_token_logprobs(lps),
                                (keys::FINISH_REASON): null
                            }]
                        });
                        Some(("".to_string(), chunk_obj))
                    }
                }
            },
            IrStreamEvent::BlockStop { .. } => None,
            IrStreamEvent::MessageDelta {
                stop_reason, usage, ..
            } => {
                // Map the IR stop_reason onto the finish_reason enum. A non-terminal delta has none and
                // serializes `null`, never the empty string (not an enum value; fails strict SDK
                // validation); only the final chunk carries a valid string. A terminal delta projects
                // the typed reason into the closed enum via the codec.
                let finish_reason: serde_json::Value = match stop_reason {
                    Some(r) => serde_json::json!(write_openai_stop_reason(*r)),
                    None => serde_json::Value::Null,
                };
                let delta_obj = serde_json::json!({});
                let mut chunk_obj = serde_json::json!({
                    (keys::OBJECT): OBJ_CHUNK,
                    (CHOICES): [{
                        (keys::INDEX): 0,
                        (keys::DELTA): delta_obj,
                        (keys::FINISH_REASON): finish_reason
                    }]
                });
                // Carry real token usage on the terminal chunk: the IR's terminal MessageDelta holds
                // the true counts, and a native include_usage stream always ends with a usage-bearing
                // chunk (omitting it loses accounting and is a tell). A top-level
                // `usage:{prompt_tokens, completion_tokens, total_tokens}` is attached.
                //
                // Native OpenAI sends a separate trailing `{choices:[], usage}` chunk, but this 1:1
                // `write_response_event` cannot return two events. So `usage` is folded onto the finish
                // chunk and the framing seam (`StreamTranslate::emit_ir_event` via
                // `split_openai_trailing_usage`) un-folds it into a native trailing chunk. Folding also
                // recovers accounting on any path that bypasses the seam. It is emitted only when a
                // count is nonzero (a same-protocol passthrough without include_usage carries zeroed
                // usage). `total_tokens` is the prompt+completion sum.
                // Rebuild the native wire shape: the IR stores uncached input but `prompt_tokens`
                // includes the cached prefix, so add `cache_read` back, with
                // `prompt_tokens_details.cached_tokens` only when a cache read is present.
                let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
                // cache_creation is also part of the total (None same-protocol; present only on
                // cross-protocol Anthropic/Bedrock ingress).
                let prompt_tokens = usage
                    .input_tokens
                    .saturating_add(cache_read)
                    .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
                let completion_tokens = usage.output_tokens;
                if prompt_tokens != 0 || completion_tokens != 0 {
                    if let Some(obj) = chunk_obj.as_object_mut() {
                        let mut usage_obj = serde_json::json!({
                            (PROMPT_TOKENS): prompt_tokens,
                            (COMPLETION_TOKENS): completion_tokens,
                            (keys::TOTAL_TOKENS): prompt_tokens.saturating_add(completion_tokens),
                        });
                        if usage.cache_read_input_tokens.is_some() {
                            if let Some(uo) = usage_obj.as_object_mut() {
                                uo.insert(
                                    PROMPT_TOKENS_DETAILS.to_string(),
                                    serde_json::json!({ (keys::CACHED_TOKENS): cache_read }),
                                );
                            }
                        }
                        // The reasoning sub-bucket (`completion_tokens_details.reasoning_tokens`, as the
                        // buffered writer), only when reported: `None` is not `Some(0)`, so a
                        // non-reasoning stream carries no fabricated details object.
                        if let Some(rt) = usage.detail.reasoning_tokens {
                            if let Some(uo) = usage_obj.as_object_mut() {
                                uo.insert(
                                    COMPLETION_TOKENS_DETAILS.to_string(),
                                    serde_json::json!({ (keys::REASONING_TOKENS): rt }),
                                );
                            }
                        }
                        obj.insert(keys::USAGE.to_string(), usage_obj);
                    }
                }
                // The serving tier rides the chunk's top level, as on a native stream (OAI-03).
                if let Some(tier) =
                    usage.detail.service_tier.as_deref().and_then(|t| {
                        crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t)
                    })
                {
                    if let Some(obj) = chunk_obj.as_object_mut() {
                        obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
                    }
                }
                Some(("".to_string(), chunk_obj))
            }
            IrStreamEvent::MessageStop => None,
            IrStreamEvent::Error(err) => {
                let message = err
                    .provider_signal
                    .clone()
                    .unwrap_or_else(|| keys::ERROR_WORD.to_string());
                // Map the IR error class onto OpenAI's enumerated error `type`; a bogus value breaks
                // clients switching on `error.type` and is a proxy tell. The match is exhaustive over
                // StatusClass so a new class forces a decision; `server_error` is the fallback.
                let error_type = match err.class {
                    busbar_contract::upstream::StatusClass::RateLimit => ERR_TYPE_RATE_LIMIT,
                    busbar_contract::upstream::StatusClass::Auth => ERR_TYPE_AUTHENTICATION,
                    // Billing exhaustion is `insufficient_quota` (HTTP 429), not `permission_error`,
                    // which OpenAI reserves for access-control denials; over-quota carries
                    // `type` and `code` both `insufficient_quota` (`bearer_error_code` pairs the
                    // code below), as in the non-stream `write_error`. Using `permission_error`
                    // misroutes quota errors and is a protocol tell.
                    busbar_contract::upstream::StatusClass::Billing => ERR_TYPE_INSUFFICIENT_QUOTA,
                    busbar_contract::upstream::StatusClass::ContextLength
                    | busbar_contract::upstream::StatusClass::ClientError => {
                        ERR_TYPE_INVALID_REQUEST
                    }
                    busbar_contract::upstream::StatusClass::Overloaded
                    | busbar_contract::upstream::StatusClass::ServerError
                    | busbar_contract::upstream::StatusClass::Timeout
                    | busbar_contract::upstream::StatusClass::Network => ERR_TYPE_SERVER_ERROR,
                };
                // `code` and `param` as JSON null, matching the native shape and the non-stream
                // `write_error` envelope; omitting them is a proxy tell and breaks clients that
                // destructure `error.code` / `error.param`.
                let error_obj = serde_json::json!({
                    (keys::ERROR_WORD): {
                        (keys::MESSAGE): message,
                        (keys::TYPE): error_type,
                        (keys::CODE): bearer_error_code(error_type),
                        (keys::PARAM): serde_json::Value::Null,
                    }
                });
                Some(("".to_string(), error_obj))
            }
        }
    }

    fn new_stream_framing(&self) -> Box<dyn super::StreamFraming> {
        // Ingress framing: replays the stream identity onto every chunk and un-folds the
        // include_usage trailer, so the agnostic translator names no OpenAI wire shape.
        Box::<OpenAiStreamFraming>::default()
    }

    fn clone_box(&self) -> Box<dyn ProtocolWriter> {
        Box::new(self.clone())
    }

    /// Native error envelope, served as `application/json`:
    /// `{"error":{"message":<msg>,"type":<type>,"param":null,"code":null}}`, the shape the SDKs
    /// decode into a typed exception. The generic `kind` maps onto OpenAI's error-`type`
    /// vocabulary where one exists, else passes through verbatim.
    fn write_error(
        &self,
        _host: &dyn busbar_contract::codec::CodecHost,
        status: u16,
        kind: &str,
        message: &str,
    ) -> serde_json::Value {
        // Map the generic `kind` onto OpenAI's error `type` ("invalid_request_error",
        // "authentication_error", "permission_error", "not_found_error", "rate_limit_error",
        // "server_error", "api_error"). Anything unrecognized falls back to a status bucket (4xx
        // invalid_request_error, 5xx server_error) so `type` is always a real OpenAI type.
        let error_type = match kind {
            ERR_TYPE_INVALID_REQUEST | "invalid_request" | "bad_request" => {
                ERR_TYPE_INVALID_REQUEST
            }
            ERR_TYPE_AUTHENTICATION | "unauthorized" | keys::AUTH_WORD => ERR_TYPE_AUTHENTICATION,
            ERR_TYPE_PERMISSION | "permission_denied" | "forbidden" => ERR_TYPE_PERMISSION,
            ERR_TYPE_NOT_FOUND => ERR_TYPE_NOT_FOUND,
            ERR_TYPE_RATE_LIMIT | "rate_limit" | "too_many_requests" => ERR_TYPE_RATE_LIMIT,
            ERR_TYPE_SERVER_ERROR | "internal_error" | "internal_server_error" => {
                ERR_TYPE_SERVER_ERROR
            }
            busbar_contract::protocol::KIND_API_ERROR => busbar_contract::protocol::KIND_API_ERROR,
            // Quota exhaustion is a native type (HTTP 429); keep `insufficient_quota` and its
            // matching `code` (set in `bearer_error_code`).
            ERR_TYPE_INSUFFICIENT_QUOTA => ERR_TYPE_INSUFFICIENT_QUOTA,
            // The all-lanes-exhausted and request-timeout 503 paths pass the Anthropic-vocabulary
            // kind `overloaded` to every ingress writer. It is not an OpenAI type (OpenAI reports a
            // 503 as `server_error`), and emitting it breaks the SDK's typed-exception mapping and
            // leaks another dialect's vocabulary. Map every transient spelling to the native 5xx type.
            busbar_contract::protocol::KIND_OVERLOADED
            | ERR_TYPE_OVERLOADED
            | "service_unavailable"
            | "unavailable"
            | "transient"
            | "timeout"
            | "network"
            | "5xx" => ERR_TYPE_SERVER_ERROR,
            busbar_contract::protocol::PROVIDER_CODE_CONTEXT_LENGTH => ERR_TYPE_INVALID_REQUEST,
            // Empty kind: derive a valid type from the status bucket, never an empty `type`.
            "" => {
                if (500..600).contains(&status) {
                    ERR_TYPE_SERVER_ERROR
                } else {
                    ERR_TYPE_INVALID_REQUEST
                }
            }
            // Any other kind (including the generic `not_found`) passes through verbatim: OpenAI
            // has no single type for it (model-not-found is `invalid_request_error` +
            // `code: "model_not_found"` on some endpoints, `not_found_error` on others).
            other => other,
        };

        serde_json::json!({
            (keys::ERROR_WORD): {
                (keys::MESSAGE): message,
                (keys::TYPE): error_type,
                (keys::PARAM): serde_json::Value::Null,
                (keys::CODE): bearer_error_code(error_type),
            }
        })
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

        // Collect the assistant text parts once: their presence decides whether `content` is null,
        // and their join is the content string.
        let text_parts: Vec<&str> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                crate::codec::ir::IrBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        // ToolUse blocks become tool_calls (not in content)
        let mut tool_calls_arr: Vec<serde_json::Value> = Vec::new();
        for block in &resp.content {
            if let crate::codec::ir::IrBlock::ToolUse {
                id, name, input, ..
            } = block
            {
                // Serialize input to JSON string
                let args_str = crate::codec::dialect::tool_arguments_to_string(input);
                tool_calls_arr.push(serde_json::json!({
                    (keys::TYPE): TOOL_TYPE_FUNCTION,
                    (keys::ID): id,
                    (keys::FUNCTION): {
                        (keys::NAME): name,
                        (keys::ARGUMENTS): args_str
                    }
                }));
            }
        }

        // Thinking blocks are dropped (the response shape has no `reasoning_content` output the
        // writer can populate) and not collapsed into content. This runs only cross-protocol
        // (same-protocol relays raw bytes and keeps `reasoning_content`), so only reasoning from
        // another source is affected. A `warn!` per non-empty block makes the loss observable.
        for block in &resp.content {
            if let crate::codec::ir::IrBlock::Thinking { text, redacted, .. } = block {
                if !text.is_empty() {
                    crate::codec::drops::writer_drop!(
                        crate::codec::drops::THINKING,
                        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                        [redacted = *redacted, byte_len = text.len(), ],
                        "dropping a reasoning/thinking block on OpenAI Chat egress: the completion \
                         response shape has no thinking output field (lossy-by-target); the \
                         chain-of-thought is NOT forwarded on this cross-protocol response");
                }
            }
        }

        // `refusal` is a required nullable member of `ChatCompletionResponseMessage`, so it is always
        // present; omitting it fails strict validation and is a proxy tell.
        //
        // When the turn ended in a refusal (`IrStopReason::Refusal`: an Anthropic `refusal` stop, or
        // this reader folding `message.refusal`), the assistant text is the refusal message,
        // written to `refusal` with `content` null, as natively. Other turns keep text in `content`
        // and `refusal: null`.
        //
        // Exact carry: when the IR flags which text is the refusal (a Chat `message.refusal`, a
        // Responses `refusal` part), only those blocks go to `refusal`. The stop-reason rule above
        // is the fallback for a source that states only that the turn was refused (Anthropic).
        let flagged = resp
            .content
            .iter()
            .any(|b| matches!(b, crate::codec::ir::IrBlock::Text { refusal: true, .. }));
        let (content_val, refusal_val) = if flagged {
            let part = |want: bool| -> serde_json::Value {
                let texts: Vec<&str> = resp
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::codec::ir::IrBlock::Text { text, refusal, .. }
                            if *refusal == want =>
                        {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(texts.concat())
                }
            };
            (part(false), part(true))
        } else {
            let refused = resp.stop_reason == Some(crate::codec::ir::IrStopReason::Refusal);
            match (!text_parts.is_empty()).then(|| text_parts.concat()) {
                Some(text) if refused => (serde_json::Value::Null, serde_json::json!(text)),
                Some(text) => (serde_json::json!(text), serde_json::Value::Null),
                None => (serde_json::Value::Null, serde_json::Value::Null),
            }
        };
        let mut message_obj = serde_json::json!({
            (keys::ROLE): keys::ASSISTANT,
            (keys::CONTENT): content_val,
            (keys::REFUSAL): refusal_val,
        });

        // Add tool_calls only if present
        if !tool_calls_arr.is_empty() {
            message_obj[keys::TOOL_CALLS] = serde_json::Value::Array(tool_calls_arr);
        }

        // Grounding sources. Chat joins every text block into one content string, so a citation's
        // offsets shift by where its block starts. Emitted only when non-empty: OpenAI omits
        // `annotations` for a completion with no sources, so a hardcoded `[]` is a tell.
        let mut base = 0usize;
        let mut annotations: Vec<serde_json::Value> = Vec::new();
        for block in &resp.content {
            if let crate::codec::ir::IrBlock::Text {
                text, citations, ..
            } = block
            {
                annotations.extend(super::super::url_citation_wire::chat_url_annotations(
                    text, base, citations,
                ));
                // Characters, not bytes: the IR citation contract is characters, and `text.len()`
                // would shift every citation in block 2+ on non-ASCII text.
                base += text.chars().count();
            }
        }
        if !annotations.is_empty() {
            message_obj[keys::ANNOTATIONS] = serde_json::Value::Array(annotations);
        }

        let mut choices_array: Vec<serde_json::Value> = Vec::new();
        // The buffered choice schema requires `finish_reason` present and a non-nullable enum over
        // {stop, length, tool_calls, content_filter, function_call}; only the stream chunk marks it
        // nullable. So neither omitting it nor JSON null is valid here. A backend that supplied no
        // stop reason (e.g. Bedrock with `stop_reason: None`) falls back to the natural-stop `stop`.
        let finish_reason: serde_json::Value = match resp.stop_reason {
            Some(r) => serde_json::json!(write_openai_stop_reason(r)),
            None => serde_json::json!(FINISH_STOP),
        };

        let mut choice_obj = serde_json::Map::new();
        choice_obj.insert(keys::INDEX.to_string(), serde_json::json!(0));
        choice_obj.insert(keys::MESSAGE.to_string(), message_obj);
        // Per-token logprobs (e.g. Gemini's `logprobsResult`) in the native choice shape.
        // `logprobs` is a required nullable member: the object when produced, else JSON null, as
        // real OpenAI returns; omitting it fails strict validation and is a tell.
        choice_obj.insert(
            keys::LOGPROBS.to_string(),
            if resp.logprobs.is_empty() {
                serde_json::Value::Null
            } else {
                crate::codec::logprob_wire::write_token_logprobs(&resp.logprobs)
            },
        );
        choice_obj.insert(keys::FINISH_REASON.to_string(), finish_reason);
        choices_array.push(serde_json::Value::Object(choice_obj));

        // Identity fields, in native order (`id`, `object`, `created`, `model`,
        // `system_fingerprint`, `choices`, `usage`). The SDKs require all but `system_fingerprint`.
        // When the IR field is `None` (cross-protocol) a correct value is synthesized.
        let id = resp.id.clone().unwrap_or_else(|| synth_completion_id(host));
        obj.insert(keys::ID.to_string(), serde_json::json!(id));
        obj.insert(keys::OBJECT.to_string(), serde_json::json!(OBJ_COMPLETION));
        let created = resp.created.unwrap_or_else(|| host_now(host));
        obj.insert(CREATED.to_string(), serde_json::json!(created));
        // model that served the response: required and non-nullable, so a backend yielding
        // `model: None` (e.g. Bedrock) would otherwise fail strict SDK deserialisation. Keep the
        // upstream value; fall back to DEFAULT_MODEL when none was supplied.
        obj.insert(
            keys::MODEL.to_string(),
            serde_json::json!(model_or_default(resp.model.as_deref())),
        );
        // system_fingerprint is emitted only when the upstream supplied one; none is fabricated.
        if let Some(ref fp) = resp.system_fingerprint {
            obj.insert(SYSTEM_FINGERPRINT.to_string(), serde_json::json!(fp));
        }
        obj.insert(CHOICES.to_string(), serde_json::Value::Array(choices_array));

        // Usage, including the `total_tokens` an SDK expects. The IR stores uncached input but
        // `prompt_tokens` includes the cached prefix, so add `cache_read` back, with
        // `prompt_tokens_details.cached_tokens` only when a cache read is present.
        let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);
        // cache_creation is also part of the total (None same-protocol; cross-protocol only).
        let prompt_tokens = resp
            .usage
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(resp.usage.cache_creation_input_tokens.unwrap_or(0));
        let mut usage_map = serde_json::Map::new();
        usage_map.insert(PROMPT_TOKENS.to_string(), serde_json::json!(prompt_tokens));
        usage_map.insert(
            COMPLETION_TOKENS.to_string(),
            serde_json::json!(resp.usage.output_tokens),
        );
        usage_map.insert(
            keys::TOTAL_TOKENS.to_string(),
            serde_json::json!(prompt_tokens.saturating_add(resp.usage.output_tokens)),
        );
        // `prompt_tokens_details` carries the cached and audio input slices: emitted when either is
        // present, each member only when reported (absent is not `Some(0)`).
        {
            let mut ptd = serde_json::Map::new();
            if resp.usage.cache_read_input_tokens.is_some() {
                ptd.insert(
                    keys::CACHED_TOKENS.to_string(),
                    serde_json::json!(cache_read),
                );
            }
            let modality = resp.usage.detail.by_modality.as_ref();
            if let Some(a) = resp
                .usage
                .detail
                .input_audio_tokens
                .or(modality.and_then(|m| m.input.audio))
            {
                ptd.insert(AUDIO_TOKENS.to_string(), serde_json::json!(a));
            }
            if let Some(t) = modality.and_then(|m| m.input.text) {
                ptd.insert(TEXT_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(i) = modality.and_then(|m| m.input.image) {
                ptd.insert(IMAGE_TOKENS.to_string(), serde_json::json!(i));
            }
            if !ptd.is_empty() {
                usage_map.insert(
                    PROMPT_TOKENS_DETAILS.to_string(),
                    serde_json::Value::Object(ptd),
                );
            }
        }
        // `completion_tokens_details` carries the reasoning, audio and predicted-output slices, with
        // the same rule: a hardcoded `0` would falsely claim the model did no thinking or audio.
        {
            let mut ctd = serde_json::Map::new();
            if let Some(rt) = resp.usage.detail.reasoning_tokens {
                ctd.insert(keys::REASONING_TOKENS.to_string(), serde_json::json!(rt));
            }
            let modality = resp.usage.detail.by_modality.as_ref();
            if let Some(a) = resp
                .usage
                .detail
                .output_audio_tokens
                .or(modality.and_then(|m| m.output.audio))
            {
                ctd.insert(AUDIO_TOKENS.to_string(), serde_json::json!(a));
            }
            if let Some(t) = modality.and_then(|m| m.output.text) {
                ctd.insert(TEXT_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(t) = resp.usage.detail.accepted_prediction_tokens {
                ctd.insert(ACCEPTED_PREDICTION_TOKENS.to_string(), serde_json::json!(t));
            }
            if let Some(t) = resp.usage.detail.rejected_prediction_tokens {
                ctd.insert(REJECTED_PREDICTION_TOKENS.to_string(), serde_json::json!(t));
            }
            if !ctd.is_empty() {
                usage_map.insert(
                    COMPLETION_TOKENS_DETAILS.to_string(),
                    serde_json::Value::Object(ctd),
                );
            }
        }
        obj.insert(
            keys::USAGE.to_string(),
            serde_json::Value::Object(usage_map),
        );
        // The tier that served the request, in OpenAI's vocabulary (OAI-03). Omitted when the
        // source named none, or a tier OpenAI has no response value for.
        if let Some(tier) = resp
            .usage
            .detail
            .service_tier
            .as_deref()
            .and_then(|t| crate::codec::carry::word_out(super::map::WORDS_SERVED_TIER, t))
        {
            obj.insert(keys::SERVICE_TIER.to_string(), serde_json::json!(tier));
        }

        serde_json::Value::Object(obj)
    }
}
