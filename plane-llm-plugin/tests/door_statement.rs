// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE DOOR'S STATEMENT, READ OFF THE RAW DOOR TABLE. The logic crate's only public item is `door`
//! (`BUSBAR-1.6.0.md` l.3560) and its root keeps the literal `#![forbid(unsafe_code)]`, so the
//! texts of the Statement and of the plane tail that need a pointer read are read here, where this
//! test crate may hold `unsafe`: the plane's name, its declaring and owned sections, its scope
//! kinds, each dialect's default outbound style, and the whole `declares` document under
//! `EXT_DECLARES`. Every expected value is stated as a literal.

use busbar_contract::abi::mechanism::call::AbiStr;
use busbar_contract::abi::mechanism::door::{
    KindTailHead, Section, Statement, SECTION_CONSUMED, SECTION_DECLARING,
};
use busbar_contract::abi::mechanism::extensions;
use busbar_contract::abi::plane::PlaneTail;

/// This plugin's own door's Statement.
fn statement() -> &'static Statement {
    // SAFETY: `busbar_plane_llm::door` answers the `'static` door of this plugin, whose Statement
    // is a `'static` const.
    unsafe { &*(*busbar_plane_llm::door()).statement }
}

/// This plugin's plane tail.
fn tail() -> &'static PlaneTail {
    let head: *const KindTailHead = statement().kind_tail;
    // SAFETY: the Statement of a plane door states its `PlaneTail` as its kind tail.
    unsafe { &*head.cast::<PlaneTail>() }
}

/// A `'static` string the door states.
fn text(s: AbiStr) -> &'static str {
    // SAFETY: every `AbiStr` the door states names a `'static` string of `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    std::str::from_utf8(bytes).expect("the door states UTF-8")
}

/// The Statement's sections.
fn sections() -> &'static [Section] {
    let st = statement();
    // SAFETY: the Statement's sections are a `'static` list of `sections_len` entries.
    unsafe { std::slice::from_raw_parts(st.sections, st.sections_len) }
}

#[test]
fn the_statement_names_the_plane_its_declaring_section_and_its_owned_one() {
    assert_eq!(text(statement().name), "llm");
    let named = |want: fn(u32) -> bool| -> Vec<&'static str> {
        sections()
            .iter()
            .filter(|s| want(s.flags))
            .map(|s| text(s.name))
            .collect()
    };
    assert_eq!(
        named(|f| f & SECTION_DECLARING != 0),
        ["pools"],
        "the one section the plane declares"
    );
    assert_eq!(
        named(|f| f & (SECTION_DECLARING | SECTION_CONSUMED) == 0),
        ["webhooks"],
        "the one section the plane owns, neither declaring nor consumed"
    );
}

#[test]
fn the_tail_states_its_scope_and_its_scope_kinds() {
    let t = tail();
    assert_eq!(text(t.scope), "pools");
    // SAFETY: the tail's `'static` scope-kind list of `scope_kinds_len` entries.
    let kinds = unsafe { std::slice::from_raw_parts(t.scope_kinds, t.scope_kinds_len) };
    let kinds: Vec<&str> = kinds.iter().map(|k| text(*k)).collect();
    assert_eq!(kinds, ["pool"]);
}

/// ARCHITECT Q-L1-AUTH (A): each dialect states its default outbound style in the tail, in the
/// order of the tail's dialects.
#[test]
fn every_dialect_states_its_default_outbound_style() {
    let t = tail();
    // SAFETY: the tail's `'static` dialect list and dialect_auth list, each of its stated length.
    let (names, lines) = unsafe {
        (
            std::slice::from_raw_parts(t.dialects, t.dialects_len),
            std::slice::from_raw_parts(t.dialect_auth, t.dialect_auth_len),
        )
    };
    let stated: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| (text(names[l.dialect as usize]), text(l.style)))
        .collect();
    assert_eq!(
        stated,
        [
            ("anthropic", "api-key"),
            ("openai", "bearer"),
            ("gemini", "x-goog-api-key"),
            ("bedrock", "sigv4"),
            ("responses", "bearer"),
            ("cohere", "bearer"),
        ]
    );
}

/// THE DECLARES RIDE THE STATEMENT (ARCHITECT ruling 2026-10-21; BUSBAR-1.6.0.md l.246): the
/// Statement's extensions state the plane's whole `declares` document under `EXT_DECLARES`, byte for
/// byte the `declares.json` the release packs.
#[test]
fn the_statement_states_the_whole_declares_document() {
    let blob = statement().extensions;
    // SAFETY: the Statement's extensions blob is `'static` bytes of its stated length.
    let bytes = unsafe { std::slice::from_raw_parts(blob.ptr, blob.len) };
    assert_eq!(
        extensions::declares(bytes),
        Some(&include_bytes!("../../plane-llm/declares.json")[..]),
    );
}
