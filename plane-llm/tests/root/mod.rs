// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE LLM PLANE DOOR'S ROOT CELLS, as the composition root's door-cells row reads them: this file
//! is the one this crate states under its `links` name (`build.rs`, `cargo::metadata=cells=`),
//! compiled as a child of the composition root's serve module in its test builds (`super::` is
//! `root::serve`).

/// THIS PLANE'S CAPABILITY-EQUALITY COLUMNS (`qa/capability-equality.json`): the cells its root leg
/// answers for.
pub(crate) const COLUMNS: &[&str] = &["llm"];

/// `(capability, column, reason)`: the root-leg cells this plane is NOT owed, each the ledger's own
/// not-applicable cell with its reason, word for word.
pub(crate) const NOT_APPLICABLE: &[(&str, &str, &str)] = &[
    (
        "trust-pinning",
        "llm",
        "LLM providers are operator-declared config endpoints, not peer-published artifacts; there is no upstream-authored schema or card to pin. The trust lifecycle governs artifacts a peer authors.",
    ),
    (
        "net-guard",
        "llm",
        "LLM egress targets are operator-configured provider endpoints; no request-authored URL reaches the LLM egress path, so there is no SSRF surface -- which is why grep finds no net_guard consumer under proxy/.",
    ),
];

/// THE SECRET-SOURCE SCAN'S DRIVEN DOOR (the composition root's `secret_source.rs`, which holds
/// every linked door plane to a driver): the Statement names the cells in this file drive.
pub(crate) fn secret_source_driven() -> Vec<String> {
    Vec::new()
}
