// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The superset intermediate representation (IR) — request and response/stream sides — that every
//! protocol's Reader/Writer maps to and from, so any ingress protocol can reach any backend
//! losslessly. (See `docs/adr/0005-ir-fidelity.md` for the fidelity contract.)
//!
//! OPACITY IS AN IR FACT, NOT A WIRE FACT. Three protocols carry assistant reasoning busbar cannot
//! decrypt, in three different shapes; [`IrBlock::is_opaque`] is the single place that knows all
//! three, so a consumer deciding "show this content or substitute a marker" never has to re-sniff
//! the wire body to get the answer right. It is a PREDICATE rather than a flag flip on
//! `Thinking.redacted` because `redacted` is a writer instruction with client-visible egress
//! consequences — the full reasoning is on `is_opaque` itself.

// G6 A4b relocation: the CONCRETE chat IR (below) + the leaf-op IR submodules moved here from
// busbar-core. The neutral `facts` trait / `handle` / `invoke` / `subscribe` stay in busbar-core
// (`busbar_contract::ir::*`); the `IrReq`/`IrResp` hub enums dissolved onto `busbar_contract::ir::handle`.
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
pub use facts_impl::project;

/// The concrete chat IR TYPES, split to `types.rs` so busbar-core can `#[path]`-share them
/// (G6 A4b). Re-exported flat so `crate::codec::ir::IrRequest` etc. keep their pre-split paths.
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
