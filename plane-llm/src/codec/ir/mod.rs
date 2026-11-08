// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The superset intermediate representation (IR), request and response/stream sides, that every
//! protocol's Reader/Writer maps to and from, so any ingress can reach any backend losslessly (see
//! `docs/adr/0005-ir-fidelity.md`).

pub mod audio;
pub mod embeddings;
/// `impl IrFacts for IrRequest` + `project` — relocated with the concrete chat IR (G6 A4b).
mod facts_impl;
pub mod image;
pub mod moderation;
pub mod open_class;
pub mod rerank;
/// IR-18: the busbar signature-provenance envelope for client carriers with no origin field.
pub mod sig_envelope;
#[cfg(test)]
pub use facts_impl::project;

/// The concrete chat IR TYPES, split to `types.rs` so busbar-core can `#[path]`-share them
/// (G6 A4b).
mod types;
pub use types::*;

/// The shape of a one-turn leaf request: one turn, no tools, no ceiling, and the text it carries.
fn one_turn_shape(
    ir: &dyn busbar_contract::ir::facts::IrFacts,
) -> busbar_contract::ir::facts::Shape {
    let items = ir.content();
    let (text_chars, system_chars) = busbar_contract::ir::facts::Shape::counts_over(&items);
    busbar_contract::ir::facts::Shape {
        turn_count: 1,
        has_tools: false,
        tool_count: 0,
        text_chars,
        system_chars,
        max_tokens: None,
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/sig_envelope_tests.rs"]
mod sig_envelope_tests;
