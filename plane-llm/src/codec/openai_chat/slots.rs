// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The irregular half of the typed IR request slots on the OpenAI Chat Completions wire.

use crate::codec::ir::{
    IrHostedTool, IrModality, IrRequest, IrToolChoice, IrUserLocation, IrVerbosity, IrWebSearch,
};
use crate::codec::keys;

/// The `tool_choice.type` of a Chat allowed-tools subset.
const TOOL_CHOICE_ALLOWED_TOOLS: &str = "allowed_tools";

/// IR-10: the allowed-tools subset of `tool_choice`, then the `tool_choice` fidelity park.
pub(super) fn read_tool_choice_slots(
    obj: &serde_json::Map<String, serde_json::Value>,
    ir: &mut IrRequest,
) {
    if let Some((names, mode)) = obj.get(keys::TOOL_CHOICE).and_then(read_allowed_tools) {
        ir.allowed_tools = Some(names);
        ir.tool_choice = Some(mode);
    }
    if let Some(raw) = obj.get(keys::TOOL_CHOICE) {
        if write_tool_choice(ir).as_ref() != Some(raw) {
            ir.extra.insert(keys::TOOL_CHOICE.to_string(), raw.clone());
        }
    }
}

/// OAI-09: a Chat `{"type":"custom","custom":{name, description?, format?}}`
pub(super) fn read_custom_tool(tool: &serde_json::Value) -> Option<IrHostedTool> {
    let obj = tool.as_object()?;
    if obj.get(keys::TYPE).and_then(|t| t.as_str()) != Some(keys::CUSTOM)
        || obj.keys().any(|k| k != keys::TYPE && k != keys::CUSTOM)
    {
        return None;
    }
    crate::codec::ir::IrCustomTool::read_members(obj.get(keys::CUSTOM)?.as_object()?, &[])
        .map(IrHostedTool::Custom)
}

/// The `tools[]` entries for the IR's custom tools, in the Chat spelling.
pub(super) fn write_custom_tools(req: &IrRequest) -> Vec<serde_json::Value> {
    req.hosted_tools
        .iter()
        .filter_map(|t| match t {
            IrHostedTool::Custom(c) => Some(serde_json::json!({
                (keys::TYPE): keys::CUSTOM,
                (keys::CUSTOM): serde_json::Value::Object(c.write_members()),
            })),
            _ => None,
        })
        .collect()
}

/// The hosted-tool kinds the Chat writer cannot express (for `dropped_egress_controls`).
pub(super) fn dropped_hosted_kinds(req: &IrRequest) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for tool in &req.hosted_tools {
        if matches!(
            tool,
            IrHostedTool::CodeExecution | IrHostedTool::WebFetch(_)
        ) {
            let k = tool.kind_str();
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// The Chat `tool_choice` the writer emits for `req` (before its no-tools guard): the allowed-tools
pub(super) fn write_tool_choice(req: &IrRequest) -> Option<serde_json::Value> {
    if let Some(names) = &req.allowed_tools {
        let mode = match req.tool_choice {
            Some(IrToolChoice::Required) => keys::REQUIRED,
            _ => keys::AUTO,
        };
        let tools: Vec<serde_json::Value> = names
            .iter()
            .map(
                |n| serde_json::json!({(keys::TYPE): super::TOOL_TYPE_FUNCTION, (keys::FUNCTION): {(keys::NAME): n}}),
            )
            .collect();
        return Some(serde_json::json!({
            (keys::TYPE): TOOL_CHOICE_ALLOWED_TOOLS,
            TOOL_CHOICE_ALLOWED_TOOLS: {(keys::MODE): mode, (keys::TOOLS): tools},
        }));
    }
    Some(match req.tool_choice.as_ref()? {
        IrToolChoice::Auto => serde_json::json!(keys::AUTO),
        IrToolChoice::None => serde_json::json!(keys::NONE_WORD),
        IrToolChoice::Required => serde_json::json!(keys::REQUIRED),
        IrToolChoice::Tool { name } => {
            serde_json::json!({(keys::TYPE): super::TOOL_TYPE_FUNCTION, (keys::FUNCTION): {(keys::NAME): name}})
        }
    })
}

/// Hook (`modalities`, IR-19): every entry must be a known word, else the slot is left absent.
pub(crate) fn read_modalities(v: &serde_json::Value, ir: &mut IrRequest) {
    ir.output_modalities = v.as_array().and_then(|arr| {
        arr.iter()
            .map(|m| m.as_str().and_then(IrModality::parse))
            .collect()
    });
}

/// Hook (`modalities`, IR-19). Chat accepts `text` and `audio`. `audio` output also REQUIRES the
pub(crate) fn write_modalities(req: &IrRequest) -> Option<serde_json::Value> {
    let has_audio_member = req.extra.contains_key(super::AUDIO);
    let mut out: Vec<serde_json::Value> = Vec::new();
    for m in req.output_modalities.as_deref()? {
        match m {
            IrModality::Text => out.push(serde_json::json!(m.as_str())),
            IrModality::Audio if has_audio_member => out.push(serde_json::json!(m.as_str())),
            IrModality::Audio | IrModality::Image => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::OUTPUT_MODALITIES,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    [modality = m.as_str(), ],
                    "dropping an output modality on OpenAI Chat egress: Chat produces image output \
                     never, and audio output only beside its own `audio` request member, which \
                     does not cross dialects");
            }
        }
    }
    (!out.is_empty()).then_some(serde_json::Value::Array(out))
}

/// Hook (`web_search_options`, IR-11): Chat `web_search_options{search_context_size,
pub(crate) fn read_web_search(v: &serde_json::Value, ir: &mut IrRequest) {
    let Some(o) = v.as_object() else {
        return;
    };
    let search_context_size = o
        .get(keys::SEARCH_CONTEXT_SIZE)
        .and_then(|s| s.as_str())
        .and_then(IrVerbosity::parse);
    let user_location = o
        .get(keys::USER_LOCATION)
        .and_then(|l| l.get(IrUserLocation::TYPE_APPROXIMATE))
        .and_then(|a| a.as_object())
        .map(IrUserLocation::read_members);
    ir.hosted_tools.push(IrHostedTool::WebSearch(IrWebSearch {
        search_context_size,
        user_location,
        ..Default::default()
    }));
}

/// Hook (`web_search_options`, IR-11): the first hosted web search, in Chat's one member. A second
pub(crate) fn write_web_search(req: &IrRequest) -> Option<serde_json::Value> {
    let mut written = None;
    for tool in &req.hosted_tools {
        match tool {
            IrHostedTool::WebSearch(search) if written.is_none() => {
                written = Some(web_search_options(search));
            }
            IrHostedTool::WebSearch(_) => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOLS,
                    &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
                    [],
                    "dropping a second hosted web search on OpenAI Chat egress: Chat has one \
                     `web_search_options` member per request"
                );
            }
            IrHostedTool::CodeExecution | IrHostedTool::WebFetch(_) => {
                crate::codec::drops::writer_drop!(
                    crate::codec::drops::TOOLS,
                    &crate::codec::diagnostics::IR_DROP_HOSTED_TOOLS,
                    [hosted_tool = tool.kind_str(),],
                    "dropping a hosted tool on OpenAI Chat egress: Chat Completions has no \
                     built-in tool of this kind (only web search, as `web_search_options`)"
                );
            }
            IrHostedTool::Custom(_) => {}
        }
    }
    written
}

/// One web search as `web_search_options`. `max_uses` and the domain filters have no Chat member:
fn web_search_options(search: &IrWebSearch) -> serde_json::Value {
    if search.max_uses.is_some()
        || !search.allowed_domains.is_empty()
        || !search.blocked_domains.is_empty()
    {
        crate::codec::drops::writer_drop!(
            crate::codec::drops::TOOLS,
            &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
            [],
            "dropping web search max_uses / domain filters on OpenAI Chat egress: \
             `web_search_options` has no member for them; the search itself is kept"
        );
    }
    let mut o = serde_json::Map::new();
    if let Some(size) = search.search_context_size {
        o.insert(
            keys::SEARCH_CONTEXT_SIZE.to_string(),
            serde_json::json!(size.as_str()),
        );
    }
    if let Some(loc) = &search.user_location {
        let approx = IrUserLocation::TYPE_APPROXIMATE;
        o.insert(
            keys::USER_LOCATION.to_string(),
            serde_json::json!({(keys::TYPE): approx, approx: serde_json::Value::Object(loc.write_members())}),
        );
    }
    serde_json::Value::Object(o)
}

/// Chat `tool_choice:{type:"allowed_tools", allowed_tools:{mode, tools:[{type:"function",
fn read_allowed_tools(v: &serde_json::Value) -> Option<(Vec<String>, IrToolChoice)> {
    if v.get(keys::TYPE).and_then(|t| t.as_str()) != Some(TOOL_CHOICE_ALLOWED_TOOLS) {
        return None;
    }
    let at = v.get(TOOL_CHOICE_ALLOWED_TOOLS)?;
    let mode = match at.get(keys::MODE).and_then(|m| m.as_str())? {
        keys::AUTO => IrToolChoice::Auto,
        keys::REQUIRED => IrToolChoice::Required,
        _ => return None,
    };
    let names: Vec<String> = at
        .get(keys::TOOLS)?
        .as_array()?
        .iter()
        .filter(|t| t.get(keys::TYPE).and_then(|x| x.as_str()) == Some(super::TOOL_TYPE_FUNCTION))
        .filter_map(|t| {
            t.get(keys::FUNCTION)
                .and_then(|f| f.get(keys::NAME))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    (!names.is_empty()).then_some((names, mode))
}
