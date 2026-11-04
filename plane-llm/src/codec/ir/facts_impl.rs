// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `impl IrFacts for IrRequest` and its `project` helper: the chat IR's projection. The neutral
//! `IrFacts` trait and `Shape`/`ContentItem`/`Slot` stay in `busbar_contract::ir::facts`.

use super::{IrBlock, IrRequest, IrRole};
use busbar_contract::ir::facts::{
    ContentItem, IrFacts, Shape, Slot, LABEL_JSON, LABEL_REASONING, OPAQUE_CONTENT_MARKER,
};
use busbar_contract::operation::OpVerb;
use std::borrow::Cow;

// Own only the projection. Opaque media and provider carry never enter a facts snapshot.
struct FactsSnapshot {
    stream: bool,
    user: Option<String>,
    shape: Shape,
    items: Vec<OwnedItem>,
}

enum OwnedItem {
    Text {
        author: &'static str,
        slot: Slot,
        text: String,
    },
    Data {
        author: &'static str,
        slot: Slot,
        label: String,
        value: serde_json::Value,
    },
    Opaque {
        author: &'static str,
        slot: Slot,
        label: &'static str,
        marker: &'static str,
    },
}

impl OwnedItem {
    fn borrowed(&self) -> ContentItem<'_> {
        match self {
            Self::Text { author, slot, text } => ContentItem::Text {
                author,
                slot: *slot,
                text: Cow::Borrowed(text),
            },
            Self::Data {
                author,
                slot,
                label,
                value,
            } => ContentItem::Data {
                author,
                slot: *slot,
                label,
                value,
            },
            Self::Opaque {
                author,
                slot,
                label,
                marker,
            } => ContentItem::Opaque {
                author,
                slot: *slot,
                label,
                marker,
            },
        }
    }
}

impl IrRequest {
    pub(crate) fn facts_snapshot(&self) -> impl IrFacts + Send + Sync + use<> {
        FactsSnapshot {
            stream: self.stream,
            user: self.user.clone(),
            shape: Shape {
                turn_count: self.messages.len() + self.system_turns_folded,
                has_tools: !self.tools.is_empty() || !self.hosted_tools.is_empty(),
                tool_count: self.tools.len() + self.hosted_tools.len(),
                max_tokens: self.max_tokens,
                ..Shape::EMPTY
            },
            items: project(self)
                .into_iter()
                .map(|item| match item {
                    ContentItem::Text { author, slot, text } => OwnedItem::Text {
                        author,
                        slot,
                        text: text.into_owned(),
                    },
                    ContentItem::Data {
                        author,
                        slot,
                        label,
                        value,
                    } => OwnedItem::Data {
                        author,
                        slot,
                        label: label.to_owned(),
                        value: value.clone(),
                    },
                    ContentItem::Opaque {
                        author,
                        slot,
                        label,
                        marker,
                    } => OwnedItem::Opaque {
                        author,
                        slot,
                        label,
                        marker,
                    },
                })
                .collect(),
        }
    }
}

impl IrFacts for FactsSnapshot {
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
        let mut shape = self.shape;
        (shape.text_chars, shape.system_chars) = Shape::counts_over(&self.content());
        shape
    }
    fn content(&self) -> Vec<ContentItem<'_>> {
        self.items.iter().map(OwnedItem::borrowed).collect()
    }
}

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
        let (text_chars, system_chars) = Shape::counts_over(&project(self));
        Shape {
            turn_count: self.messages.len() + self.system_turns_folded,
            has_tools: !self.tools.is_empty() || !self.hosted_tools.is_empty(),
            tool_count: self.tools.len() + self.hosted_tools.len(),
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
/// stream.
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
    // Sort only coverage ranges; the original folds retain their conversation order below.
    let mut ranges: Vec<_> = req.system_folds.iter().map(|f| clamp(&f.blocks)).collect();
    ranges.sort_unstable_by_key(|r| r.start);
    let mut ranges = ranges.into_iter().peekable();
    let mut covered_until = 0;
    for (b, block) in req.system.iter().enumerate() {
        while let Some(range) = ranges.next_if(|r| r.start <= b) {
            if range.start < range.end {
                covered_until = covered_until.max(range.end);
            }
        }
        if b >= covered_until {
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
/// one place `IrRole` becomes the neutral opaque label, with the walk, not the neutral gate that
/// reads it.
fn author_of(role: IrRole) -> &'static str {
    match role {
        IrRole::System => "system",
        IrRole::User => "user",
        IrRole::Assistant => "assistant",
        IrRole::Tool => "tool",
    }
}

/// One level of the block walk. `turn` is `None` for the system slot, where a tool call or result
/// has no turn to be attributed to and keeps the slot it was reached through.
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

#[cfg(test)]
#[path = "tests/allocation_work_tests.rs"]
mod allocation_work_tests;
