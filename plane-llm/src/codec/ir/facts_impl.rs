// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `impl IrFacts for IrRequest` and its `project` helper: the chat IR's projection. The neutral

use super::{IrBlock, IrRequest, IrRole};
use busbar_contract::ir::facts::{
    ContentItem, IrFacts, Shape, Slot, LABEL_JSON, LABEL_REASONING, OPAQUE_CONTENT_MARKER,
};
use busbar_contract::operation::OpVerb;
use std::borrow::Cow;

impl IrFacts for IrRequest {
    fn verb(&self) -> OpVerb {
        OpVerb::CHAT
    }

    fn wants_stream(&self) -> bool {
        self.stream
    }

    fn end_user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    fn shape(&self) -> Shape {
        let mut text_chars = 0usize;
        let mut system_chars = 0usize;
        for item in project(self) {
            let n = item.screenable_text().chars().count();
            text_chars += n;
            if matches!(item.slot(), Slot::System) {
                system_chars += n;
            }
        }
        Shape {
            turn_count: self.messages.len() + self.system_turns_folded,
            has_tools: !self.tools.is_empty(),
            tool_count: self.tools.len(),
            text_chars,
            system_chars,
            max_tokens: self.max_tokens,
        }
    }

    fn content(&self) -> Vec<ContentItem<'_>> {
        project(self)
    }
}

/// The projection: walk one chat-family IR request into the flat, ordered, protocol-blind item

pub fn project(req: &IrRequest) -> Vec<ContentItem<'_>> {
    let mut out = Vec::new();
    if req.system_folds.is_empty() {
        walk(
            &req.system,
            author_of(IrRole::System),
            None,
            Slot::System,
            &mut out,
        );
        for (i, m) in req.messages.iter().enumerate() {
            turn(&m.content, author_of(m.role), i, &mut out);
        }
        return out;
    }

    let len = req.system.len();
    let clamp = |r: &std::ops::Range<usize>| r.start.min(len)..r.end.min(len);
    for (b, block) in req.system.iter().enumerate() {
        if !req
            .system_folds
            .iter()
            .any(|f| clamp(&f.blocks).contains(&b))
        {
            walk(
                std::slice::from_ref(block),
                author_of(IrRole::System),
                None,
                Slot::System,
                &mut out,
            );
        }
    }
    let mut folds = req.system_folds.iter().peekable();
    let mut t = 0;
    for i in 0..=req.messages.len() {
        while let Some(f) = folds.next_if(|f| f.before <= i) {
            let blocks = req.system.get(clamp(&f.blocks)).unwrap_or_default();
            turn(blocks, f.role.as_str(), t, &mut out);
            t += 1;
        }
        if let Some(m) = req.messages.get(i) {
            turn(&m.content, author_of(m.role), t, &mut out);
            t += 1;
        }
    }
    out
}

/// One conversation turn at index `i`, under the empty-turn rule (see [`project`]).
fn turn<'a>(blocks: &'a [IrBlock], author: &'static str, i: usize, out: &mut Vec<ContentItem<'a>>) {
    let before = out.len();
    walk(blocks, author, Some(i), Slot::Turn(i), out);
    if out.len() == before {
        out.push(ContentItem::Text {
            author,
            slot: Slot::Turn(i),
            text: Cow::Borrowed(""),
        });
    }
}

/// The LLM family's author label for a chat role, the exact strings the hook wire has carried. The

fn author_of(role: IrRole) -> &'static str {
    match role {
        IrRole::System => "system",
        IrRole::User => "user",
        IrRole::Assistant => "assistant",
        IrRole::Tool => "tool",
    }
}

/// One level of the block walk. `turn` is `None` for the system slot, where a tool call or result

fn walk<'a>(
    blocks: &'a [IrBlock],
    author: &'static str,
    turn: Option<usize>,
    slot: Slot,
    out: &mut Vec<ContentItem<'a>>,
) {
    for b in blocks {
        match b {
            IrBlock::Text { text, .. } => out.push(ContentItem::Text {
                author,
                slot,
                text: Cow::Borrowed(text.as_str()),
            }),
            IrBlock::Thinking { text, .. } => {
                if b.is_opaque() {
                    out.push(ContentItem::Opaque {
                        author,
                        slot,
                        label: LABEL_REASONING,
                        marker: OPAQUE_CONTENT_MARKER,
                    });
                } else {
                    out.push(ContentItem::Text {
                        author,
                        slot,
                        text: Cow::Borrowed(text.as_str()),
                    });
                }
            }
            IrBlock::ToolUse { name, input, .. } => out.push(ContentItem::Data {
                author,
                slot: turn.map_or(slot, Slot::ToolArgs),
                label: name.as_str(),
                value: input,
            }),
            IrBlock::ToolResult { content, .. } => walk(
                content,
                author,
                turn,
                turn.map_or(slot, Slot::ToolResult),
                out,
            ),

            IrBlock::Image { .. } | IrBlock::Media { .. } => {}

            IrBlock::HostedToolRecord { .. } => {}
            IrBlock::Json(v) => out.push(ContentItem::Data {
                author,
                slot,
                label: LABEL_JSON,
                value: v,
            }),
        }
    }
}

#[cfg(test)]
#[path = "tests/facts_tests.rs"]
mod tests;
