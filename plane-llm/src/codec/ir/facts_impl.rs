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
        // One walk over the same items a content-granted hook is shown, so the total and the
        // system-only subtotal cannot drift from each other or the content (see `ContentItem::screenable_text`).
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
            // The wire turn count: an in-band system turn is a turn (`project` puts it back at its wire position).
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
/// stream. System slot first, then each turn in order; within a turn, blocks in the reader's order
/// (which the writer sends). A consumer rebuilds per-turn text by grouping on [`Slot::turn_index`]
/// and joining [`ContentItem::screenable_text`].
///
/// # The empty-turn rule
///
/// A turn that yields no items still yields one empty [`ContentItem::Text`]. A media-only turn must
/// not vanish: a screening hook seeing three turns where the provider sees four has been told the
/// request is something it is not. Expressed against `IrRequest::messages` plus the in-band system
/// turns the reader folded (`IrRequest::system_folds`), each at its wire position.
///
/// # Opacity is checked before `text` is read
///
/// [`IrBlock::Thinking`]'s `text` is not always plaintext: for two dialects' redacted shapes it
/// holds provider ciphertext, and for a third it is empty while the blob rides `signature`. A naive
/// `if redacted { marker } else { text }` shows a hook an empty turn for a request the provider
/// receives in full, a policy-enforcement bypass. [`IrBlock::is_opaque`] knows all three shapes and
/// is asked first.
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
    // The in-band system turn is a turn. A dialect carrying its system prompt inside the turn array
    // shows it to a hook as a turn, at its wire position and written role (the system slot holds
    // only the dialect's own system field). The reader folded it into `system`; `system_folds` records
    // where it stood. Without this a `prompt: rw` hook's reply, which replaces the whole turn array,
    // deletes the operator's system prompt upstream. Turn indices are wire positions, folds included.
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
        // The empty-turn rule (see `project`).
        out.push(ContentItem::Text {
            author,
            slot: Slot::Turn(i),
            text: Cow::Borrowed(""),
        });
    }
}

/// The LLM family's author label for a chat role, the exact strings the hook wire has carried. The
/// one place `IrRole` becomes the neutral opaque label, with the walk, not the neutral gate that
/// reads it. Pinned by `role_label_is_byte_identical_to_the_seam` in the ir role tests.
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
                    // Reasoning busbar can read is ordinary screenable content: a client replaying a
                    // multi-turn body sends it to the provider, so a gate that cannot see it has a hole.
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
            // Structurally text-less and deliberately not widened (see the module header). `Media`
            // joins `Image` on the same decision, made explicitly: widening would newly disclose
            // attachment bytes, mime types and filenames to every content hook and sidecar, a real
            // disclosure change to decide on its own terms in its own diff, as for image provenance.
            IrBlock::Image { .. } | IrBlock::Media { .. } => {}
            // A provider-run tool's record is closed likewise: its search results are not the
            // conversation's text, and opening them to hooks is its own disclosure decision.
            IrBlock::HostedToolRecord { .. } => {}
            IrBlock::Json(v) => out.push(ContentItem::Data {
                author,
                slot,
                label: LABEL_JSON,
                value: v,
            }),
            // No catch-all, on purpose: a new IR block variant must be decided here, in a diff a
            // reviewer reads, not default to invisible (the failure direction this module closes).
        }
    }
}

#[cfg(test)]
#[path = "tests/facts_tests.rs"]
mod tests;
