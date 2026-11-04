// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Unit coverage for [`busbar_contract::ir::facts`] — the properties the projection owes, asserted against IR
//! values built by hand so that a failure here names the projection and never a reader.

use super::*;
use crate::codec::ir::{CacheControl, CacheKind, IrImageSource, IrMessage};
use serde_json::json;

fn text(t: &str) -> IrBlock {
    IrBlock::text(t.to_string())
}

fn turn(role: IrRole, content: Vec<IrBlock>) -> IrMessage {
    IrMessage { role, content }
}

fn req(system: Vec<IrBlock>, messages: Vec<IrMessage>) -> IrRequest {
    IrRequest {
        system,
        messages,
        ..IrRequest::default()
    }
}

/// The flat rendering a consumer builds: system text, then one `(role, joined text)` per turn.
fn flatten(r: &IrRequest) -> (Option<String>, Vec<(&'static str, String)>) {
    let items = project(r);
    let sys: Vec<String> = items
        .iter()
        .filter(|i| i.slot() == Slot::System)
        .map(|i| i.screenable_text().into_owned())
        .collect();
    let mut turns: Vec<(&'static str, Vec<String>)> = Vec::new();
    for i in &items {
        let Some(idx) = i.slot().turn_index() else {
            continue;
        };
        while turns.len() <= idx {
            turns.push(("user", Vec::new()));
        }
        turns[idx].0 = i.author();
        turns[idx].1.push(i.screenable_text().into_owned());
    }
    (
        Some(sys.join("\n")).filter(|s| !s.is_empty()),
        turns.into_iter().map(|(r, p)| (r, p.join("\n"))).collect(),
    )
}

/// # THE BYPASS THAT MUST NOT REOPEN
///
/// All three opaque reasoning shapes project the MARKER, and none of them projects its payload.
#[test]
fn every_opaque_reasoning_shape_projects_the_marker_and_never_its_payload() {
    let shapes = [
        (
            "flagged, ciphertext in text",
            IrBlock::Thinking {
                text: "OPAQUE_CIPHERTEXT_BYTES".to_string(),
                signature: None,
                redacted: true,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            "OPAQUE_CIPHERTEXT_BYTES",
        ),
        (
            "flagged, ciphertext in text, signed",
            IrBlock::Thinking {
                text: "OPAQUE_BEDROCK_BYTES".to_string(),
                signature: Some("sig".to_string()),
                redacted: true,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            "OPAQUE_BEDROCK_BYTES",
        ),
        (
            "unflagged, blob on the signature",
            IrBlock::Thinking {
                text: String::new(),
                signature: Some("OPAQUE_BLOB_XYZ".to_string()),
                redacted: false,
                cache_control: None,
                kind: None,
                signature_origin: None,
            },
            "OPAQUE_BLOB_XYZ",
        ),
    ];

    for (name, block, payload) in shapes {
        let r = req(Vec::new(), vec![turn(IrRole::Assistant, vec![block])]);
        let items = project(&r);
        assert_eq!(items.len(), 1, "{name}: one block, one item");
        assert!(
            matches!(items[0], ContentItem::Opaque { .. }),
            "{name}: an opaque block must project as Opaque, not as text — a consumer has to be \
             able to tell 'nothing here' from 'something here I cannot show you'"
        );
        assert_eq!(
            items[0].screenable_text(),
            OPAQUE_CONTENT_MARKER,
            "{name}: the marker stands in for the payload"
        );
        assert_eq!(
            items[0].author(),
            "assistant",
            "{name}: an opaque blob still has an author, and a policy asks who wrote it first"
        );
        let rendered = flatten(&r).1[0].1.clone();
        assert!(
            !rendered.contains(payload),
            "{name}: the payload reached the projection. That is a NEW DISCLOSURE to the \
             operator's hook sidecar, and for the unflagged shape it is the fixed bypass reopened."
        );
    }
}

/// Reasoning busbar CAN read is ordinary content and is shown. A client replaying a multi-turn body
/// sends this text to the provider, so a gate that cannot see it is a gate with a hole in it.
#[test]
fn readable_reasoning_is_projected_as_text() {
    let r = req(
        Vec::new(),
        vec![turn(
            IrRole::Assistant,
            vec![
                IrBlock::Thinking {
                    text: "CHAIN OF THOUGHT".to_string(),
                    signature: Some("sig".to_string()),
                    redacted: false,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                },
                text("the answer"),
            ],
        )],
    );
    assert_eq!(
        flatten(&r).1,
        vec![("assistant", "CHAIN OF THOUGHT\nthe answer".to_string())],
        "block order is preserved and readable reasoning joins the turn's text"
    );
}

/// A turn whose every block is structurally text-less still appears. The old contract's LETTER
/// (index alignment with the wire `messages` array) cannot survive the move onto the IR; its
/// PURPOSE — a screening hook sees every turn — is asserted here against `IrRequest::messages`.
#[test]
fn a_turn_with_no_projectable_content_still_appears() {
    let r = req(
        Vec::new(),
        vec![
            turn(IrRole::User, vec![text("before")]),
            turn(
                IrRole::User,
                vec![IrBlock::Image {
                    source: IrImageSource::Url("https://example.test/a.png".to_string()),
                    cache_control: None,
                    detail: None,
                }],
            ),
            turn(IrRole::Assistant, vec![text("after")]),
        ],
    );
    assert_eq!(
        flatten(&r).1,
        vec![
            ("user", "before".to_string()),
            ("user", String::new()),
            ("assistant", "after".to_string()),
        ],
        "the media-only turn keeps its entry, with its role, and reads as empty rather than \
         vanishing — a hook told there are two turns where the provider sees three has been told \
         the request is something it is not"
    );
    assert_eq!(r.shape().turn_count, 3);
}

/// Tool-call ARGUMENTS and tool-result CONTENT are projected, attributed to the turn that carried
/// them, and distinguishable from ordinary prose by their slot.
#[test]
fn tool_arguments_and_results_carry_the_turn_they_belong_to() {
    let r = req(
        Vec::new(),
        vec![
            turn(IrRole::User, vec![text("run it")]),
            turn(
                IrRole::Assistant,
                vec![IrBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "search".to_string(),
                    input: json!({"q": "ARGUMENT PAYLOAD"}),
                    cache_control: None,
                    thought_signature: None,
                }],
            ),
            turn(
                IrRole::Tool,
                vec![IrBlock::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: vec![text("TOOL RESULT PAYLOAD")],
                    is_error: false,
                    cache_control: None,
                }],
            ),
        ],
    );
    let items = project(&r);
    let slots: Vec<Slot> = items.iter().map(ContentItem::slot).collect();
    assert_eq!(
        slots,
        vec![Slot::Turn(0), Slot::ToolArgs(1), Slot::ToolResult(2)],
        "each tool slot names the TURN it belongs to, so a guardrail can attribute a payload to \
         the exchange that produced it"
    );
    assert_eq!(
        slots.iter().map(|s| s.turn_index()).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)],
    );
    assert!(
        items[1].screenable_text().contains("ARGUMENT PAYLOAD"),
        "the arguments are visible; nothing projected them before"
    );
    assert_eq!(items[2].screenable_text(), "TOOL RESULT PAYLOAD");
    assert_eq!(
        items[2].author(),
        "tool",
        "the result keeps the authoring role of the turn that carried it"
    );
}

/// The system slot is its own slot, never turn 0, on every dialect — because on the IR there is one
/// place a system prompt lives and the reader already put it there.
#[test]
fn the_system_prompt_is_its_own_slot_and_never_a_turn() {
    let r = req(
        vec![text("one"), text("two")],
        vec![turn(IrRole::User, vec![text("hi")])],
    );
    let (system, turns) = flatten(&r);
    assert_eq!(system.as_deref(), Some("one\ntwo"));
    assert_eq!(turns, vec![("user", "hi".to_string())]);
    assert_eq!(
        r.shape().turn_count,
        1,
        "the system prompt does not inflate the turn count"
    );
}

/// A request with no system prompt yields no system items at all — "absent" and "present but empty"
/// stay distinguishable, which is what a hook keys its grant detection off.
#[test]
fn an_absent_system_prompt_projects_no_system_items() {
    let r = req(Vec::new(), vec![turn(IrRole::User, vec![text("hi")])]);
    assert_eq!(flatten(&r).0, None);
    assert!(project(&r).iter().all(|i| i.slot() != Slot::System));
}

/// # THE TRIPWIRE THAT IS NOW A STRUCTURAL FACT
///
/// `Shape::text_chars` is a SUM over the same items a content-granted hook is shown, so the size
/// signal and the content projection cannot drift apart.
#[test]
fn text_chars_is_exactly_what_a_content_granted_hook_is_shown() {
    let r = req(
        vec![text("SYSTEM")],
        vec![
            turn(IrRole::User, vec![text("hello"), text("again")]),
            turn(
                IrRole::Assistant,
                vec![IrBlock::Thinking {
                    text: "ciphertext".to_string(),
                    signature: None,
                    redacted: true,
                    cache_control: None,
                    kind: None,
                    signature_origin: None,
                }],
            ),
            turn(
                IrRole::User,
                vec![IrBlock::Image {
                    source: IrImageSource::Url("https://example.test/a.png".to_string()),
                    cache_control: None,
                    detail: None,
                }],
            ),
        ],
    );
    let shown: usize = project(&r)
        .iter()
        .map(|i| i.screenable_text().chars().count())
        .sum();
    assert_eq!(r.shape().text_chars, shown);
    assert_eq!(
        r.shape().text_chars,
        6 + 5 + 5 + OPAQUE_CONTENT_MARKER.chars().count(),
        "system + both text blocks + the marker standing in for the opaque one; the media-only \
         turn's empty entry contributes nothing"
    );
}

/// The `max_tokens` signal is the READER's normalized value, whatever field the dialect spelled it
/// in.
#[test]
fn the_shape_signals_come_from_the_normalized_ir() {
    let r = IrRequest {
        messages: vec![turn(IrRole::User, vec![text("hi")])],
        max_tokens: Some(128),
        tools: Vec::new(),
        stream: true,
        user: Some("alice".to_string()),
        ..IrRequest::default()
    };
    assert_eq!(r.shape().max_tokens, Some(128));
    assert!(!r.shape().has_tools);
    assert!(r.wants_stream());
    assert_eq!(r.end_user(), Some("alice"));
    assert_eq!(r.verb(), OpVerb::CHAT);
    assert_eq!(
        r.content(),
        project(&r),
        "the family-blind seam and the family's own walk are the SAME projection — the trait is \
         where a second protocol family plugs in, never a second answer for this one"
    );
}

/// The marker is BYTE-IDENTICAL to the one the raw-body projection substitutes today.
#[test]
fn opaque_marker_is_byte_identical_to_the_projection_it_replaces() {
    assert_eq!(OPAQUE_CONTENT_MARKER, "[busbar:redacted_reasoning]");
}

/// Structured tool-result members are content, not silence. A dialect's `{"json": …}`
#[test]
fn a_structured_tool_result_member_is_projected_as_data() {
    let r = req(
        Vec::new(),
        vec![turn(
            IrRole::Tool,
            vec![IrBlock::ToolResult {
                tool_use_id: "c1".to_string(),
                content: vec![IrBlock::Json(json!({"rows": ["UNTRUSTED"]}))],
                is_error: false,
                cache_control: None,
            }],
        )],
    );
    let items = project(&r);
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0],
        ContentItem::Data {
            label: LABEL_JSON,
            slot: Slot::ToolResult(0),
            ..
        }
    ));
    assert!(items[0].screenable_text().contains("UNTRUSTED"));
}

/// A cache breakpoint on a block changes nothing about what is projected. Recorded because
/// `cache_control` is the field most likely to be added to a block in a future unit, and a walk that
/// accidentally keyed on it would be a silent content-visibility change.
#[test]
fn cache_breakpoints_do_not_change_what_is_projected() {
    let r = req(
        Vec::new(),
        vec![turn(
            IrRole::User,
            vec![IrBlock::Text {
                text: "hello".to_string(),
                cache_control: Some(CacheControl {
                    kind: CacheKind::Ephemeral,
                }),
                citations: Vec::new(),
                refusal: false,
            }],
        )],
    );
    assert_eq!(flatten(&r).1, vec![("user", "hello".to_string())]);
}

#[test]
fn screening_digest_distinguishes_the_same_text_in_different_slots() {
    let a = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(0),
        text: "payload".into(),
    };
    let b = ContentItem::Text {
        author: "user",
        slot: Slot::ToolResult(0),
        text: "payload".into(),
    };
    assert_ne!(a.screening_digest(), b.screening_digest());
}

#[test]
fn screening_digest_distinguishes_authors() {
    let a = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(0),
        text: "x".into(),
    };
    let b = ContentItem::Text {
        author: "assistant",
        slot: Slot::Turn(0),
        text: "x".into(),
    };
    assert_ne!(a.screening_digest(), b.screening_digest());
}

/// Without length-prefix framing, `("ab","c")` and `("a","bc")` would hash the same concatenated
/// bytes — and one session's cleared-set could skip-screen the other piece.
#[test]
fn screening_digest_framing_prevents_a_concatenation_collision() {
    let a = ContentItem::Text {
        author: "ab",
        slot: Slot::Turn(0),
        text: "c".into(),
    };
    let b = ContentItem::Text {
        author: "a",
        slot: Slot::Turn(0),
        text: "bc".into(),
    };
    assert_ne!(a.screening_digest(), b.screening_digest());
}

/// The VARIANT is part of a piece's identity. A `Text` body that happens to equal a `Data`
#[test]
fn screening_digest_distinguishes_variants_with_identical_text() {
    let value = serde_json::json!({"a": 1});
    let serialized = value.to_string();
    let text = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(0),
        text: serialized.clone().into(),
    };
    let data = ContentItem::Data {
        author: "user",
        slot: Slot::Turn(0),
        label: "args",
        value: &value,
    };

    assert_eq!(text.screenable_text(), data.screenable_text());
    assert_ne!(text.screening_digest(), data.screening_digest());

    let marker = "[redacted]";
    let opaque_as_text = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(0),
        text: marker.into(),
    };
    let opaque = ContentItem::Opaque {
        author: "user",
        slot: Slot::Turn(0),
        label: "reasoning",
        marker,
    };
    assert_eq!(opaque_as_text.screenable_text(), opaque.screenable_text());
    assert_ne!(opaque_as_text.screening_digest(), opaque.screening_digest());
}

#[test]
fn screening_digest_is_deterministic() {
    let a = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(3),
        text: "same".into(),
    };
    let b = ContentItem::Text {
        author: "user",
        slot: Slot::Turn(3),
        text: "same".into(),
    };
    assert_eq!(a.screening_digest(), b.screening_digest());
}

#[test]
fn shape_counts_hosted_tool_declarations() {
    let mut request = IrRequest::default();
    request
        .hosted_tools
        .push(crate::codec::ir::IrHostedTool::CodeExecution);
    assert!(request.shape().has_tools);
    assert_eq!(request.shape().tool_count, 1);
    request
        .hosted_tools
        .push(crate::codec::ir::IrHostedTool::WebSearch(Default::default()));
    assert_eq!(request.shape().tool_count, 2);
}
