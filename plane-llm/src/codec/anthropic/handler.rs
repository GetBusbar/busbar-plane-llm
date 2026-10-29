// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Anthropic `RequestHandler`. Chat-only today (Anthropic ships no embeddings/images/audio API here);
//! its non-chat operations stay `None` = no-handler 404. Chat dispatches through the same registry as
//! every other operation.

use busbar_contract::codec::EgressCtx;
use busbar_contract::codec::RequestHandler;
use busbar_contract::operation::OpVerb;

/// Endpoint paths — each appears on BOTH the egress side (`upstream_path`) and the ingress match
/// (`resolve_operation`); single-sourced so the two sides cannot drift.
const PATH_MESSAGES: &str = "/v1/messages";

pub struct AnthropicRequestHandler;
/// This protocol's OWN chat instance — delete this line (and the registry arm) and this
/// protocol's chat 404s via the standard no-handler path; everything else keeps working.
static CHAT: super::super::chat_handle::ChatOperation =
    super::super::chat_handle::ChatOperation(super::VENDOR_NAME);

/// Anthropic's row of the support matrix: one verb. Every other verb is the standard no-handler 404.
static CELLS: &[busbar_contract::codec::Cell] = &[(OpVerb::CHAT, &CHAT)];

impl RequestHandler for AnthropicRequestHandler {
    dialect_identity!(super::VENDOR_NAME);
    fn upstream_path(&self, ctx: &EgressCtx) -> String {
        match ctx.path_base {
            // Claude-on-Vertex: the model rides the URL via `:rawPredict` / `:streamRawPredict`; the
            // body change (drop `model`, add `anthropic_version`) is applied at wire finalization.
            Some(base) => {
                let verb = if ctx.stream {
                    "streamRawPredict"
                } else {
                    "rawPredict"
                };
                format!("{base}/{}:{verb}", ctx.model)
            }
            // Native Messages API (chat only); streaming is negotiated via the `stream` flag + SSE Accept.
            None => PATH_MESSAGES.into(),
        }
    }
    fn resolve_operation(&self, path: &str, _body: &[u8]) -> Option<OpVerb> {
        path.ends_with(PATH_MESSAGES).then_some(OpVerb::CHAT)
    }
}

#[cfg(test)]
#[path = "tests/handler_tests.rs"]
mod tests;
